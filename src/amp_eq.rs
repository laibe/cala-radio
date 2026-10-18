//! The TAS58xx amplifier's 15-band equalizer, reached through ALSA controls.
//!
//! The driver (sonocotta/tas5805m-driver-for-raspbian) registers one integer
//! mixer control per band, named by its centre frequency, plus an
//! `Equalizer` On/Off switch. These are ordinary ALSA controls, so this goes
//! through libasound directly rather than shelling out to `amixer` — which
//! DietPi removes again whenever its sound card setting is re-applied.

use std::ffi::CStr;
use std::io;

use alsa::ctl::{ElemId, ElemIface};
use alsa::hctl::Elem;
use alsa::HCtl;

use crate::eq::BANDS;

/// The amplifier's ALSA card. By name rather than index, so the EQ can never
/// be written to some other card that happens to enumerate first.
pub const CARD: &str = "hw:LouderRaspberry";

/// Band controls in [`crate::eq`] order, lowest frequency first.
const BAND_CONTROLS: [&CStr; BANDS] = [
    c"00020 Hz",
    c"00032 Hz",
    c"00050 Hz",
    c"00080 Hz",
    c"00125 Hz",
    c"00200 Hz",
    c"00315 Hz",
    c"00500 Hz",
    c"00800 Hz",
    c"01250 Hz",
    c"02000 Hz",
    c"03150 Hz",
    c"05000 Hz",
    c"08000 Hz",
    c"16000 Hz",
];

const EQUALIZER_SWITCH: &CStr = c"Equalizer";

pub struct AmpEq {
    hctl: HCtl,
}

impl AmpEq {
    /// Open the amplifier's controls and check that every band is there.
    pub fn open() -> io::Result<Self> {
        let hctl = HCtl::new(CARD, false).map_err(|e| io::Error::other(format!("{CARD}: {e}")))?;
        hctl.load().map_err(io::Error::other)?;
        let amp = Self { hctl };
        for name in BAND_CONTROLS {
            amp.elem(name)?;
        }
        Ok(amp)
    }

    /// Current value of every band, in dB.
    pub fn read_bands(&self) -> io::Result<[i32; BANDS]> {
        let mut bands = [0; BANDS];
        for (band, name) in bands.iter_mut().zip(BAND_CONTROLS) {
            let value = self.elem(name)?.read().map_err(io::Error::other)?;
            *band = value.get_integer(0).ok_or_else(|| not_integer(name))?;
        }
        Ok(bands)
    }

    /// Set every band, in dB. Bands already at their value are skipped, so
    /// the driver only talks to the amp for bands that actually change.
    pub fn write_bands(&self, bands: &[i32; BANDS]) -> io::Result<()> {
        for (&db, name) in bands.iter().zip(BAND_CONTROLS) {
            let elem = self.elem(name)?;
            let mut value = elem.read().map_err(io::Error::other)?;
            if value.get_integer(0) == Some(db) {
                continue;
            }
            value.set_integer(0, db).ok_or_else(|| not_integer(name))?;
            elem.write(&value).map_err(io::Error::other)?;
        }
        Ok(())
    }

    /// Whether the `Equalizer` switch is On, if it can be read. With it Off
    /// the band controls still accept values but have no audible effect.
    pub fn equalizer_enabled(&self) -> Option<bool> {
        let value = self.elem(EQUALIZER_SWITCH).ok()?.read().ok()?;
        // Item 0 is "On", item 1 is "Off".
        value.get_enumerated(0).map(|item| item == 0)
    }

    fn elem(&self, name: &CStr) -> io::Result<Elem<'_>> {
        let mut id = ElemId::new(ElemIface::Mixer);
        id.set_name(name);
        self.hctl.find_elem(&id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{CARD} has no control {name:?}"),
            )
        })
    }
}

fn not_integer(name: &CStr) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{name:?} is not an integer control"),
    )
}
