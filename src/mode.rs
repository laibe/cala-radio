//! Application mode selection via the Fermion 8421 absolute encoder.
//!
//! The 8421 encoder is a 16-position absolute encoder. We map a subset of
//! positions to the three application modes:
//!
//! | Position | Bits (8:4:2:1) | Mode     |
//! |----------|----------------|----------|
//! | `0x8`    | 8:1 4:0 2:0 1:0 | Radio    |
//! | `0x7`    | 8:0 4:1 2:1 1:1 | Clock    |
//! | `0x6`    | 8:0 4:1 2:1 1:0 | Streamer (Spotify Connect) |
//!
//! Any other position falls back to [`Mode::Radio`] as a safe default.

/// The three application operating modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Internet radio playback.
    Radio,
    /// Clock showing the local (Berlin) time.
    Clock,
    /// Spotify Connect receiver.
    Streamer,
}

impl Mode {
    /// The absolute 8421 encoder position for this mode.
    pub const fn position(self) -> u8 {
        match self {
            Mode::Radio => 0x8,
            Mode::Clock => 0x7,
            Mode::Streamer => 0x6,
        }
    }

    /// Map an absolute 8421 position to a mode, defaulting to [`Mode::Radio`].
    pub const fn from_position(position: u8) -> Self {
        match position {
            0x8 => Mode::Radio,
            0x7 => Mode::Clock,
            0x6 => Mode::Streamer,
            _ => Mode::Radio,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_known_positions() {
        assert_eq!(Mode::from_position(0x8), Mode::Radio);
        assert_eq!(Mode::from_position(0x7), Mode::Clock);
        assert_eq!(Mode::from_position(0x6), Mode::Streamer);
    }

    #[test]
    fn defaults_unknown_to_radio() {
        for pos in [0x0, 0x1, 0x5, 0x9, 0xF] {
            assert_eq!(Mode::from_position(pos), Mode::Radio, "pos {pos:#x}");
        }
    }

    #[test]
    fn round_trip_positions() {
        for mode in [Mode::Radio, Mode::Clock, Mode::Streamer] {
            assert_eq!(Mode::from_position(mode.position()), mode);
        }
    }
}
