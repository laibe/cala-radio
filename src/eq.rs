//! Tone control: three knob-sized groups on top of the amplifier's 15-band EQ.
//!
//! The TAS58xx amp exposes one control per band (20 Hz to 16 kHz, each -15 to
//! +15 dB). Fifteen controls is a lot for a single knob, so the bands are
//! split into three groups of five — lows, mids and highs — and the knob moves
//! a whole group up or down.
//!
//! The knob adds an *offset* to a base curve rather than overwriting bands, so
//! a hand-tuned shape (say, a bump at 315 Hz) survives being turned up or
//! down. Both are kept in a small text file so the curve comes back after a
//! restart; see [`EqState::load`] and [`EqState::save`].
//!
//! Hardware access lives in `amp_eq`; everything here is pure and testable.

use std::fs;
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

/// Number of EQ bands on the amplifier.
pub const BANDS: usize = 15;

/// Range of a single band, as the driver accepts it.
pub const BAND_MIN_DB: i32 = -15;
pub const BAND_MAX_DB: i32 = 15;

/// How far the knob can move a group away from its base curve.
///
/// Deliberately well inside the band range. The EQ is applied digitally
/// before the power stage, so a big boost eats headroom and clips at higher
/// volumes; ±6 dB is plenty to shape the tone of a small speaker.
pub const OFFSET_LIMIT_DB: i32 = 6;

const FILE_HEADER: &str = "\
# EQ curve saved by the radio.
# base: the 15 amplifier bands (20 Hz to 16 kHz) in dB, -15 to 15.
# lows/mids/highs: the EQ knob's offset for each group, -6 to 6.
# Delete this file to adopt the amplifier's current curve on the next start.
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Lows,
    Mids,
    Highs,
}

impl Group {
    pub const ALL: [Group; 3] = [Group::Lows, Group::Mids, Group::Highs];

    /// The group a click moves to, wrapping from highs back to lows.
    pub fn next(self) -> Self {
        match self {
            Group::Lows => Group::Mids,
            Group::Mids => Group::Highs,
            Group::Highs => Group::Lows,
        }
    }

    /// Name for the display and the state file.
    pub fn label(self) -> &'static str {
        match self {
            Group::Lows => "LOWS",
            Group::Mids => "MIDS",
            Group::Highs => "HIGHS",
        }
    }

    /// Indices of this group's bands: 20–125 Hz, 200–1250 Hz, 2–16 kHz.
    pub fn bands(self) -> Range<usize> {
        match self {
            Group::Lows => 0..5,
            Group::Mids => 5..10,
            Group::Highs => 10..15,
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// The base curve plus the knob's offset for each group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EqState {
    base: [i32; BANDS],
    offsets: [i32; 3],
}

impl EqState {
    /// Start from `base` with every group at zero. Out-of-range bands are
    /// clamped.
    pub fn from_base(base: [i32; BANDS]) -> Self {
        Self {
            base: base.map(clamp_band),
            offsets: [0; 3],
        }
    }

    pub fn offset(&self, group: Group) -> i32 {
        self.offsets[group.index()]
    }

    /// Move `group` by `delta_db`, limited to ±[`OFFSET_LIMIT_DB`]. Returns
    /// the new offset.
    pub fn adjust(&mut self, group: Group, delta_db: i32) -> i32 {
        let offset = &mut self.offsets[group.index()];
        *offset = offset
            .saturating_add(delta_db)
            .clamp(-OFFSET_LIMIT_DB, OFFSET_LIMIT_DB);
        *offset
    }

    /// The value for each band: its base plus its group's offset, clamped to
    /// what the driver accepts.
    pub fn bands(&self) -> [i32; BANDS] {
        let mut out = self.base;
        for group in Group::ALL {
            for band in group.bands() {
                out[band] = clamp_band(self.base[band] + self.offset(group));
            }
        }
        out
    }

    /// Parse the state file format written by [`Self::to_text`].
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut base = None;
        let mut offsets = [0; 3];
        for (i, line) in text.lines().enumerate() {
            let n = i + 1;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut words = line.split_whitespace();
            let Some(key) = words.next() else {
                continue;
            };
            let values = words
                .map(|w| {
                    w.parse::<i32>()
                        .map_err(|e| format!("line {n}: {w:?}: {e}"))
                })
                .collect::<Result<Vec<_>, _>>()?;

            if key == "base" {
                let bands: [i32; BANDS] = values.try_into().map_err(|v: Vec<i32>| {
                    format!("line {n}: expected {BANDS} band values, got {}", v.len())
                })?;
                base = Some(bands.map(clamp_band));
            } else if let Some(group) = Group::ALL
                .into_iter()
                .find(|g| g.label().eq_ignore_ascii_case(key))
            {
                let &[value] = values.as_slice() else {
                    return Err(format!("line {n}: expected one value for {key}"));
                };
                offsets[group.index()] = value.clamp(-OFFSET_LIMIT_DB, OFFSET_LIMIT_DB);
            } else {
                return Err(format!("line {n}: unknown key {key:?}"));
            }
        }
        let base = base.ok_or("missing base line")?;
        Ok(Self { base, offsets })
    }

    pub fn to_text(&self) -> String {
        let base: Vec<String> = self.base.iter().map(i32::to_string).collect();
        let mut text = format!("{FILE_HEADER}base {}\n", base.join(" "));
        for group in Group::ALL {
            text.push_str(&format!(
                "{} {}\n",
                group.label().to_ascii_lowercase(),
                self.offset(group)
            ));
        }
        text
    }

    /// Read a saved state. A missing file is reported as
    /// [`io::ErrorKind::NotFound`]; a malformed one as `InvalidData`.
    pub fn load(path: &Path) -> io::Result<Self> {
        let text = fs::read_to_string(path)?;
        Self::parse(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Write the state to `path`.
    ///
    /// Written to a temporary file and renamed into place, so a power cut
    /// mid-write leaves the previous curve rather than a truncated file.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(self.to_text().as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&tmp, path)
    }
}

fn clamp_band(db: i32) -> i32 {
    db.clamp(BAND_MIN_DB, BAND_MAX_DB)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURVE: [i32; BANDS] = [2, 2, 2, 2, 2, 3, 5, 3, 0, 0, 0, 0, 0, 0, 2];

    #[test]
    fn click_cycles_through_the_groups() {
        assert_eq!(Group::Lows.next(), Group::Mids);
        assert_eq!(Group::Mids.next(), Group::Highs);
        assert_eq!(Group::Highs.next(), Group::Lows);
    }

    #[test]
    fn groups_cover_every_band_once() {
        let mut seen = [0; BANDS];
        for group in Group::ALL {
            for band in group.bands() {
                seen[band] += 1;
            }
        }
        assert_eq!(seen, [1; BANDS]);
    }

    /// The point of offsets: turning a group keeps the hand-tuned shape.
    #[test]
    fn offset_moves_a_group_and_keeps_its_shape() {
        let mut eq = EqState::from_base(CURVE);
        eq.adjust(Group::Mids, 2);
        assert_eq!(eq.bands(), [2, 2, 2, 2, 2, 5, 7, 5, 2, 2, 0, 0, 0, 0, 2]);
    }

    #[test]
    fn offset_is_limited() {
        let mut eq = EqState::from_base(CURVE);
        assert_eq!(eq.adjust(Group::Lows, 100), OFFSET_LIMIT_DB);
        assert_eq!(eq.adjust(Group::Lows, -1), OFFSET_LIMIT_DB - 1);
        assert_eq!(eq.adjust(Group::Highs, -100), -OFFSET_LIMIT_DB);
        assert_eq!(eq.offset(Group::Mids), 0);
    }

    #[test]
    fn bands_never_leave_the_driver_range() {
        let mut eq = EqState::from_base([14; BANDS]);
        eq.adjust(Group::Lows, OFFSET_LIMIT_DB);
        let bands = eq.bands();
        assert!(bands
            .iter()
            .all(|b| (BAND_MIN_DB..=BAND_MAX_DB).contains(b)));
        assert_eq!(bands[0], BAND_MAX_DB);
        assert_eq!(bands[5], 14);
    }

    #[test]
    fn text_round_trips() {
        let mut eq = EqState::from_base(CURVE);
        eq.adjust(Group::Lows, 3);
        eq.adjust(Group::Highs, -2);
        assert_eq!(EqState::parse(&eq.to_text()).unwrap(), eq);
    }

    #[test]
    fn missing_group_lines_default_to_zero() {
        let eq = EqState::parse("base 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\nmids 4\n").unwrap();
        assert_eq!(eq.offset(Group::Lows), 0);
        assert_eq!(eq.offset(Group::Mids), 4);
        assert_eq!(eq.offset(Group::Highs), 0);
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let eq = EqState::parse("base 99 0 0 0 0 0 0 0 0 0 0 0 0 0 -99\nlows 20\n").unwrap();
        assert_eq!(eq.offset(Group::Lows), OFFSET_LIMIT_DB);
        assert_eq!(eq.bands()[0], BAND_MAX_DB);
        assert_eq!(eq.bands()[14], BAND_MIN_DB);
    }

    #[test]
    fn malformed_files_are_rejected() {
        assert!(EqState::parse("").is_err(), "no base");
        assert!(EqState::parse("base 1 2 3\n").is_err(), "short base");
        assert!(EqState::parse("base 0 0 0 0 0 0 0 0 0 0 0 0 0 0 x\n").is_err());
        let flat = EqState::from_base([0; BANDS]).to_text();
        assert!(EqState::parse(&format!("{flat}treble 2\n")).is_err());
        assert!(EqState::parse(&format!("{flat}lows 1 2\n")).is_err());
    }

    #[test]
    fn save_then_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("eq.txt");
        let mut eq = EqState::from_base(CURVE);
        eq.adjust(Group::Mids, -4);
        eq.save(&path).unwrap();
        assert_eq!(EqState::load(&path).unwrap(), eq);
        assert!(!dir.path().join("eq.txt.tmp").exists());

        // Saving again replaces the file rather than appending to it.
        eq.adjust(Group::Mids, 1);
        eq.save(&path).unwrap();
        assert_eq!(EqState::load(&path).unwrap(), eq);
    }

    #[test]
    fn missing_file_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = EqState::load(&dir.path().join("eq.txt")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
