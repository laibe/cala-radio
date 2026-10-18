//! Adafruit Stemma QT rotary encoders (product 5880) on the shared I2C bus.
//!
//! See HARDWARE.md for the I2C addresses:
//! - Select: `0x36`
//! - Volume: `0x37`
//! - EQ:     `0x3A`
//!
//! All three knobs are initialized; any of them may be missing at runtime.
//!
//! ## Failure handling
//!
//! A knob that fails to initialize, or that starts failing its reads, is
//! dropped and retried periodically instead of being kept around as a dead
//! device that logs a warning on every poll. The radio keeps working with
//! whatever knobs are present — losing a knob must never be fatal, and a knob
//! that comes back (a nudged Stemma cable, a brown-out on the bus) should
//! start working again without a restart.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use adafruit_seesaw::devices::RotaryEncoder;
use adafruit_seesaw::prelude::*;
use adafruit_seesaw::{Driver, SeesawDriver};
use embedded_hal_bus::i2c::RefCellDevice;
use linux_embedded_hal::{Delay, I2cdev};
use tracing::{debug, info, warn};

pub const ADDR_SELECT: u8 = 0x36;
pub const ADDR_VOLUME: u8 = 0x37;
pub const ADDR_EQ: u8 = 0x3A;

/// Consecutive read failures before we assume the device is gone and re-init.
const ERROR_THRESHOLD: u32 = 20;

/// How often to retry a knob that is currently missing.
const RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Seesaw NeoPixel module registers. The crate's `NeopixelModule` needs
/// nightly (`generic_const_exprs`), so the pixel is driven directly.
const NEOPIXEL_PIN: &[u8; 2] = &[0x0E, 0x01];
const NEOPIXEL_BUF_LENGTH: &[u8; 2] = &[0x0E, 0x03];
const NEOPIXEL_BUF: &[u8; 2] = &[0x0E, 0x04];
const NEOPIXEL_SHOW: &[u8; 2] = &[0x0E, 0x05];
/// The encoder's single NeoPixel is on seesaw pin 6.
const NEOPIXEL_PIN_NUMBER: u8 = 6;
/// Settle time after configuring the NeoPixel pin and buffer, in µs.
const NEOPIXEL_CONFIG_DELAY_US: u32 = 10_000;

pub type SeesawBus<'a> = SeesawDriver<RefCellDevice<'a, I2cdev>, Delay>;

/// One rotary knob, which may be absent.
struct Knob<'a> {
    addr: u8,
    label: &'static str,
    device: Option<RotaryEncoder<SeesawBus<'a>>>,
    errors: u32,
    next_retry: Instant,
}

impl<'a> Knob<'a> {
    fn new(addr: u8, label: &'static str, i2c: &'a RefCell<I2cdev>, now: Instant) -> Self {
        let mut knob = Self {
            addr,
            label,
            device: None,
            errors: 0,
            next_retry: now,
        };
        knob.try_init(i2c, now);
        knob
    }

    fn try_init(&mut self, i2c: &'a RefCell<I2cdev>, now: Instant) {
        if self.device.is_some() || now < self.next_retry {
            return;
        }
        self.next_retry = now + RETRY_INTERVAL;
        let encoder =
            RotaryEncoder::new(self.addr, SeesawDriver::new(Delay, RefCellDevice::new(i2c)));
        match encoder.init() {
            Ok(mut encoder) => {
                // The knobs sit inside the housing, so keep the pixel dark.
                if let Err(e) = blank_neopixel(&mut encoder) {
                    debug!("{} encoder NeoPixel blank failed: {e:?}", self.label);
                }
                info!("{} encoder at 0x{:02X} initialized", self.label, self.addr);
                self.device = Some(encoder);
                self.errors = 0;
            }
            Err(e) => {
                warn!(
                    "{} encoder at 0x{:02X} init failed: {e} (retrying in {:?})",
                    self.label, self.addr, RETRY_INTERVAL
                );
            }
        }
    }

    /// Run a read, folding a failure into the health counter.
    fn read<T, E: std::fmt::Debug>(
        &mut self,
        op: impl FnOnce(&mut RotaryEncoder<SeesawBus<'a>>) -> Result<T, E>,
    ) -> Option<T> {
        let device = self.device.as_mut()?;
        match op(device) {
            Ok(value) => {
                self.errors = 0;
                Some(value)
            }
            Err(e) => {
                self.errors += 1;
                if self.errors >= ERROR_THRESHOLD {
                    warn!(
                        "{} encoder at 0x{:02X} failed {} reads in a row ({e:?}); will re-init",
                        self.label, self.addr, self.errors
                    );
                    self.device = None;
                    self.errors = 0;
                } else {
                    debug!("{} encoder read failed: {e:?}", self.label);
                }
                None
            }
        }
    }

    fn is_present(&self) -> bool {
        self.device.is_some()
    }
}

/// Turn off the encoder's RGB NeoPixel (GRB, one pixel).
fn blank_neopixel<D: Driver>(encoder: &mut RotaryEncoder<D>) -> Result<(), D::Error> {
    let addr = encoder.addr();
    let driver = encoder.driver();
    driver.register_write_with_delay(
        addr,
        NEOPIXEL_PIN,
        &[NEOPIXEL_PIN_NUMBER],
        NEOPIXEL_CONFIG_DELAY_US,
    )?;
    driver.register_write_with_delay(
        addr,
        NEOPIXEL_BUF_LENGTH,
        &3u16.to_be_bytes(),
        NEOPIXEL_CONFIG_DELAY_US,
    )?;
    // Buffer offset 0, then G, R, B.
    driver.register_write(addr, NEOPIXEL_BUF, &[0x00, 0x00, 0x00, 0x00, 0x00])?;
    driver.register_write(addr, NEOPIXEL_SHOW, &[])
}

/// All rotary knobs on the shared I2C bus.
pub struct Knobs<'a> {
    i2c: &'a RefCell<I2cdev>,
    select: Knob<'a>,
    volume: Knob<'a>,
    eq: Knob<'a>,
}

impl<'a> Knobs<'a> {
    /// Initialize the Select, Volume and EQ knobs on the shared I2C bus.
    ///
    /// Never fails: knobs that are not answering are retried in the
    /// background by [`Self::service`].
    pub fn init(i2c: &'a RefCell<I2cdev>) -> Self {
        let now = Instant::now();
        Self {
            i2c,
            select: Knob::new(ADDR_SELECT, "Select", i2c, now),
            volume: Knob::new(ADDR_VOLUME, "Volume", i2c, now),
            eq: Knob::new(ADDR_EQ, "EQ", i2c, now),
        }
    }

    /// Re-init any knob that is currently missing. Cheap to call every tick.
    pub fn service(&mut self, now: Instant) {
        self.select.try_init(self.i2c, now);
        self.volume.try_init(self.i2c, now);
        self.eq.try_init(self.i2c, now);
    }

    pub fn any_present(&self) -> bool {
        self.select.is_present() || self.volume.is_present() || self.eq.is_present()
    }

    /// Relative movement of the EQ knob since the last call.
    pub fn eq_delta(&mut self) -> Option<i32> {
        self.eq.read(|d| d.delta(0))
    }

    /// Raw (un-debounced) level of the EQ knob's push button.
    pub fn eq_button(&mut self) -> Option<bool> {
        self.eq.read(|d| d.button(0))
    }

    /// Relative movement of the volume knob since the last call.
    pub fn volume_delta(&mut self) -> Option<i32> {
        self.volume.read(|d| d.delta(0))
    }

    /// Raw (un-debounced) level of the select knob's push button.
    pub fn select_button(&mut self) -> Option<bool> {
        self.select.read(|d| d.button(0))
    }

    /// Absolute position of the select knob.
    pub fn select_position(&mut self) -> Option<i32> {
        self.select.read(|d| d.position(0))
    }
}
