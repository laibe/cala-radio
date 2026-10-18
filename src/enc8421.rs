//! Async driver for the Fermion 8421 absolute position encoder.
//!
//! The driver is intentionally tiny:
//!
//! * [`Encoder::spawn`] reserves the four BCD bit pins, takes an initial
//!   debounced reading, and spawns a tokio task that polls every 20 ms.
//! * Any task can read the current position lock-free via
//!   [`Encoder::current`].
//!
//! ## Why polling?
//!
//! The Fermion is an *absolute* encoder: each detent produces a complete
//! 4-bit code, so a missed sample only loses intermediate positions, never
//! the truth. Polling at ~50 Hz is simpler, easier to debounce, and CPU cost
//! is negligible on a Pi Zero. Reach for interrupts only when working with
//! incremental (Gray / quadrature) encoders.
//!
//! ## Why tokio?
//!
//! Matches the radio crate's runtime, and the polling task is a natural fit
//! for a spawned task with an interval.
//
// On macOS the Linux-gated helpers and bit-decode helpers look "unused";
// silence those warnings so `cargo check` stays clean in CI/dev.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[cfg(target_os = "linux")]
use rppal::gpio::{Gpio, InputPin};
use tokio::task::JoinHandle;
#[cfg_attr(not(target_os = "linux"), allow(unused_imports))]
use tokio::time::{interval, MissedTickBehavior};
#[cfg_attr(not(target_os = "linux"), allow(unused_imports))]
use tracing::{debug, instrument};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// BCM GPIO pin numbers for the four BCD bit pins.
///
/// The physical wiring is `Pi pin 13 -> bit 1`, etc. — see HARDWARE.md.
pub mod pin {
    pub const BIT_1: u8 = 27;
    pub const BIT_2: u8 = 22;
    pub const BIT_4: u8 = 20;
    pub const BIT_8: u8 = 17;
}

/// How fast the driver polls. 20 ms = 50 Hz: plenty for a hand turned by a
/// human, far below any CPU budget concern.
const POLL_PERIOD: Duration = Duration::from_millis(20);

/// Debounce: a value is accepted once this many consecutive samples taken
/// `DEBOUNCE_GAP` apart agree.
const STABLE_COUNT: u8 = 3;
const DEBOUNCE_GAP: Duration = Duration::from_millis(2);

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync + 'static>>;

// ---------------------------------------------------------------------------
// Pins
// ---------------------------------------------------------------------------

/// Owns the four `InputPin`s. Drops them at program exit so the kernel
/// releases the GPIO lines.
#[cfg(target_os = "linux")]
struct Pins {
    b1: InputPin,
    b2: InputPin,
    b4: InputPin,
    b8: InputPin,
}

#[cfg(target_os = "linux")]
impl Pins {
    fn acquire() -> Result<Self> {
        let gpio = Gpio::new()?;
        Ok(Self {
            b1: gpio.get(pin::BIT_1)?.into_input_pulldown(),
            b2: gpio.get(pin::BIT_2)?.into_input_pulldown(),
            b4: gpio.get(pin::BIT_4)?.into_input_pulldown(),
            b8: gpio.get(pin::BIT_8)?.into_input_pulldown(),
        })
    }

    /// Snapshot the four bit inputs. Cheap; reads are direct MMIO for
    /// `InputPin::is_high()` (no syscalls).
    fn raw(&self) -> RawBits {
        RawBits {
            b1: self.b1.is_high(),
            b2: self.b2.is_high(),
            b4: self.b4.is_high(),
            b8: self.b8.is_high(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct RawBits {
    b1: bool,
    b2: bool,
    b4: bool,
    b8: bool,
}

impl RawBits {
    /// "8:1 4:1 2:0 1:1" style debug string, MSB first.
    fn fmt_short(&self) -> String {
        format!(
            "8:{} 4:{} 2:{} 1:{}",
            u8::from(self.b8),
            u8::from(self.b4),
            u8::from(self.b2),
            u8::from(self.b1),
        )
    }
}

fn decode(bits: &RawBits) -> u8 {
    (u8::from(bits.b1))
        | (u8::from(bits.b2) << 1)
        | (u8::from(bits.b4) << 2)
        | (u8::from(bits.b8) << 3)
}

/// Poll the pins until `STABLE_COUNT` consecutive samples within
/// `DEBOUNCE_GAP` agree. Returns the position and the bits observed in the
/// most recent agreed-upon sample.
#[cfg(target_os = "linux")]
async fn stable_read(pins: &Pins) -> (u8, RawBits) {
    let initial = pins.raw();
    let (mut pos, mut bits) = (decode(&initial), initial);
    let mut matches: u8 = 1;

    while matches < STABLE_COUNT {
        tokio::time::sleep(DEBOUNCE_GAP).await;
        let r = pins.raw();
        let p = decode(&r);
        if p == pos {
            matches += 1;
            bits = r;
        } else {
            pos = p;
            bits = r;
            matches = 1;
        }
    }

    (pos, bits)
}

// ---------------------------------------------------------------------------
// Public driver
// ---------------------------------------------------------------------------

/// Cheaply cloneable handle to the running driver.
///
/// Cloning is `Arc::clone` — give copies to every task that needs the
/// position.
#[derive(Clone)]
pub struct Encoder {
    inner: Arc<Inner>,
}

struct Inner {
    position: Arc<AtomicU8>,
    /// Kept so the polling task lives exactly as long as the handle does.
    _task: JoinHandle<()>,
}

impl Encoder {
    /// Reserve the four bit pins, take an initial debounced reading, and
    /// spawn the polling task.
    ///
    /// Must be called inside a tokio runtime context (e.g. inside a
    /// `#[tokio::main]` function or after `tokio::runtime::Handle::current()`
    /// is valid).
    #[instrument]
    pub async fn spawn() -> Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let pins = Pins::acquire()?;
            let (initial, init_bits) = stable_read(&pins).await;
            debug!(
                position = initial,
                bits = %init_bits.fmt_short(),
                "encoder initial stable reading"
            );

            let position = Arc::new(AtomicU8::new(initial));
            let task_position = position.clone();

            let _task = tokio::spawn(async move {
                run_polling_loop(pins, init_bits, initial, task_position).await;
            });

            Ok(Self {
                inner: Arc::new(Inner { position, _task }),
            })
        }

        #[cfg(not(target_os = "linux"))]
        {
            Err("enc8421: driver only supported on Linux/Pi \
                 (rppal uses /dev/gpiomem)"
                .into())
        }
    }

    /// Current position (0..=15). Lock-free, safe to call from any task.
    pub fn current(&self) -> u8 {
        self.inner.position.load(Ordering::Relaxed)
    }
}

/// Internal polling loop. Runs until the runtime shuts down.
#[cfg(target_os = "linux")]
#[instrument(skip(pins), fields(initial))]
async fn run_polling_loop(pins: Pins, initial_bits: RawBits, initial: u8, position: Arc<AtomicU8>) {
    let mut last = initial;
    debug!(
        last = last,
        bits = %initial_bits.fmt_short(),
        "encoder polling started"
    );

    let mut ticker = interval(POLL_PERIOD);
    // If the executor stalls and ticks queue up, don't fire many at once.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;
        let (pos, bits) = stable_read(&pins).await;
        if pos != last {
            debug!(
                from = last,
                to = pos,
                bits = %bits.fmt_short(),
                "encoder changed"
            );
            position.store(pos, Ordering::Relaxed);
            last = pos;
        }
    }
}
