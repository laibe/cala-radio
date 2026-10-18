//! The front panel: display, rotary knobs, and mode selector.
//!
//! This runs on its own OS thread. I2C transactions block, and the cpal handle
//! inside the audio output is not `Send`, so neither belongs on a tokio worker.
//! The thread talks to the async world through the tokio [`Handle`] it was
//! given, plus channels.
//!
//! Every peripheral here is optional. A radio that keeps playing with a dead
//! display is a far better outcome than one that refuses to start, and a knob
//! that starts answering again after a loose Stemma cable is re-seated should
//! just work without a restart.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use embedded_hal_bus::i2c::RefCellDevice;
use linux_embedded_hal::I2cdev;
use nicolai_vfd::Vfd;
use tokio::runtime::Handle;
use tracing::{debug, error, info, warn};

use radio::amp_eq::AmpEq;
use radio::debounce::{Button, Edge};
use radio::enc8421::Encoder;
use radio::encoders::Knobs;
use radio::eq::{EqState, Group};
use radio::mode::Mode;
use radio::player::RodioPlayer;
use radio::radio_list::RadioList;
use radio::spotify::SpotifyHandle;
use radio::watchdog::Watchdog;
use radio::RadioApp;

use crate::{clock_display_text, eq_display_text, pad_display, volume_display_text};

/// Panel poll period.
///
/// 50 Hz is far more than a human hand needs while keeping bus traffic low.
/// Note that this is not what fixes button bounce — see [`radio::debounce`]
/// for that.
const POLL_PERIOD: Duration = Duration::from_millis(20);

/// How long a volume or EQ change stays on the display before it reverts.
const OVERLAY: Duration = Duration::from_secs(2);

/// Volume change per detent of the knob.
const VOLUME_STEP: f32 = 0.1;

/// EQ change per detent of the knob, in dB.
const EQ_STEP_DB: i32 = 1;

/// Minimum gap between EQ writes to the amplifier.
///
/// Every changed band is an I2C transaction from the driver to the amp, on
/// the same bus as the knobs and display, and each detent moves five bands.
/// Coalescing keeps a quick spin from swamping the bus.
const EQ_WRITE_INTERVAL: Duration = Duration::from_millis(100);

/// How long the EQ must be left alone before the state file is rewritten.
/// Saving on every detent would mean dozens of SD card writes per spin.
const EQ_SAVE_DELAY: Duration = Duration::from_secs(5);

/// How long the select knob must be still before we actually tune in.
///
/// The display follows the knob immediately; only the connection waits. This
/// keeps a spin through the list from opening a connection per station, which
/// stations can reasonably treat as abuse.
const STATION_SETTLE: Duration = Duration::from_millis(400);

/// How long to keep retrying the audio device before giving up and letting
/// systemd restart us. ALSA is not always ready the instant the unit starts.
const AUDIO_INIT_DEADLINE: Duration = Duration::from_secs(60);

/// How long to keep retrying the I2C bus before continuing without a panel.
const I2C_INIT_DEADLINE: Duration = Duration::from_secs(20);

type VfdDevice<'a> = Vfd<RefCellDevice<'a, I2cdev>>;

// ---------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------

/// The VFD, plus the text we believe is on it.
struct Display<'a> {
    vfd: Option<VfdDevice<'a>>,
    shown: Option<String>,
}

impl<'a> Display<'a> {
    fn new(vfd: Option<VfdDevice<'a>>) -> Self {
        Self { vfd, shown: None }
    }

    /// Write `text` if it differs from what is already up.
    ///
    /// A failed write leaves `shown` untouched, so the next tick retries
    /// rather than leaving the panel permanently stale.
    fn show(&mut self, text: &str) {
        if self.shown.as_deref() == Some(text) {
            return;
        }
        let Some(vfd) = self.vfd.as_mut() else {
            self.shown = Some(text.to_string());
            return;
        };
        // No clear() first: every string we are given is exactly the panel
        // width in bytes, so the write overwrites all of it. Clearing would
        // double the I2C traffic and make the display flicker on every update.
        debug_assert_eq!(text.len(), crate::DISPLAY_WIDTH);
        if let Err(e) = vfd.write_text(text) {
            debug!("VFD write failed: {e:?}");
            return;
        }
        self.shown = Some(text.to_string());
    }

    fn blank(&mut self) {
        if let Some(vfd) = self.vfd.as_mut() {
            let _ = vfd.clear();
        }
        self.shown = None;
    }
}

// ---------------------------------------------------------------------------
// Initialization — none of it fatal except the audio device
// ---------------------------------------------------------------------------

fn open_audio(
    mono: bool,
    handle: &Handle,
    shutdown: &AtomicBool,
    watchdog: &Watchdog,
) -> Option<RodioPlayer> {
    let deadline = Instant::now() + AUDIO_INIT_DEADLINE;
    let mut backoff = Duration::from_secs(1);
    loop {
        {
            // `RodioPlayer::new` captures `Handle::current()`, which is how
            // `play` can later be driven from this non-runtime thread.
            let _guard = handle.enter();
            match RodioPlayer::new(mono) {
                Ok(player) => return Some(player),
                Err(e) => warn!("Audio device not available: {e}"),
            }
        }
        if shutdown.load(Ordering::Relaxed) || Instant::now() >= deadline {
            error!("Giving up on the audio device; exiting so the supervisor can restart us");
            return None;
        }
        // Keep the watchdog fed: waiting for ALSA is progress, not a hang.
        watchdog.ping();
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

fn open_i2c(shutdown: &AtomicBool, watchdog: &Watchdog) -> Option<RefCell<I2cdev>> {
    let deadline = Instant::now() + I2C_INIT_DEADLINE;
    loop {
        match I2cdev::new("/dev/i2c-1") {
            Ok(dev) => return Some(RefCell::new(dev)),
            Err(e) => debug!("I2C bus not ready: {e}"),
        }
        if shutdown.load(Ordering::Relaxed) || Instant::now() >= deadline {
            // Not fatal: audio still works, the mode selector still works, and
            // the radio plays. Only the panel is missing.
            error!("No I2C bus at /dev/i2c-1; continuing without display or knobs");
            return None;
        }
        watchdog.ping();
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn init_vfd(i2c: &RefCell<I2cdev>) -> Option<VfdDevice<'_>> {
    let mut vfd = Vfd::new(RefCellDevice::new(i2c));
    if let Err(e) = vfd.set_offset(0) {
        warn!("VFD did not respond: {e:?} — continuing without a display");
        return None;
    }
    let _ = vfd.clear();
    // Display on, test mode off, LED off: the board's amber LED is hidden in
    // the housing anyway.
    if let Err(e) = vfd.set_control(true, false, false) {
        debug!("VFD set_control failed: {e:?}");
    }
    info!("Display initialized");
    Some(vfd)
}

fn init_mode_encoder(handle: &Handle) -> Option<Encoder> {
    let _guard = handle.enter();
    match handle.block_on(Encoder::spawn()) {
        Ok(encoder) => {
            info!("Mode selector ready (position 0x{:X})", encoder.current());
            Some(encoder)
        }
        Err(e) => {
            // Usually means the GPIO lines are still claimed by a previous
            // run. Radio mode is the safe default; the selector is not
            // retried, but a restart clears it and playback is unaffected.
            error!("Mode selector unavailable ({e}); locked to Radio mode");
            None
        }
    }
}

/// Wait for `main` to hand over the Spotify control surface, giving up if we
/// are asked to shut down first.
fn wait_for_spotify(rx: &Receiver<SpotifyHandle>, shutdown: &AtomicBool) -> Option<SpotifyHandle> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if shutdown.load(Ordering::Relaxed) {
            return None;
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(handle) => return Some(handle),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    warn!("Spotify receiver unavailable; Streamer mode will be silent");
    None
}

// ---------------------------------------------------------------------------
// Tone — the EQ knob driving the amplifier's equalizer
// ---------------------------------------------------------------------------

/// The selected EQ group, the curve, and changes not yet sent to the amp or
/// saved to the state file.
struct Tone {
    amp: AmpEq,
    state: EqState,
    path: PathBuf,
    group: Group,
    /// The amp has not been sent the current curve yet.
    unsent: bool,
    next_write: Instant,
    save_at: Option<Instant>,
}

impl Tone {
    /// Open the amplifier's EQ and restore the saved curve.
    ///
    /// Not fatal: without the TAS58xx controls the radio plays as before and
    /// the EQ knob only says so.
    fn init(path: &Path, now: Instant) -> Option<Self> {
        let amp = match AmpEq::open() {
            Ok(amp) => amp,
            Err(e) => {
                warn!("Amplifier EQ unavailable ({e}); the EQ knob is disabled");
                return None;
            }
        };
        if amp.equalizer_enabled() == Some(false) {
            warn!("The amplifier's Equalizer control is Off; EQ changes will not be audible");
        }

        let state = match EqState::load(path) {
            Ok(state) => {
                info!("EQ restored from {}", path.display());
                state
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    info!(
                        "No EQ file at {}; adopting the amplifier's current curve",
                        path.display()
                    );
                } else {
                    warn!(
                        "Could not read EQ file {} ({e}); adopting the amplifier's current curve",
                        path.display()
                    );
                }
                let base = match amp.read_bands() {
                    Ok(base) => base,
                    Err(e) => {
                        warn!("Could not read the amplifier's EQ ({e}); the EQ knob is disabled");
                        return None;
                    }
                };
                let state = EqState::from_base(base);
                // Save straight away. If the knob changed the amp and we
                // crashed before saving, re-reading the base from the amp on
                // the next start would bake those offsets into it.
                if let Err(e) = state.save(path) {
                    warn!("Could not save EQ to {}: {e}", path.display());
                }
                state
            }
        };

        Some(Self {
            amp,
            state,
            path: path.to_path_buf(),
            group: Group::Lows,
            // Send the restored curve on the first tick: after a reboot the
            // amp holds whatever alsactl restored, not our curve.
            unsent: true,
            next_write: now,
            save_at: None,
        })
    }

    /// Move to the next group, returning it with its current offset.
    fn next_group(&mut self) -> (Group, i32) {
        self.group = self.group.next();
        (self.group, self.state.offset(self.group))
    }

    /// Turn the selected group by `detents`, returning it with its new offset.
    fn adjust(&mut self, detents: i32, now: Instant) -> (Group, i32) {
        let offset = self
            .state
            .adjust(self.group, detents.saturating_mul(EQ_STEP_DB));
        self.unsent = true;
        self.save_at = Some(now + EQ_SAVE_DELAY);
        (self.group, offset)
    }

    /// Send pending changes to the amp and the state file once they are due.
    fn service(&mut self, now: Instant) {
        if self.unsent && now >= self.next_write {
            self.next_write = now + EQ_WRITE_INTERVAL;
            self.send();
        }
        if self.save_at.is_some_and(|at| now >= at) {
            self.save_at = None;
            self.save();
        }
    }

    /// Write out anything still pending, on shutdown.
    fn flush(&mut self) {
        if self.unsent {
            self.send();
        }
        if self.save_at.take().is_some() {
            self.save();
        }
    }

    fn send(&mut self) {
        match self.amp.write_bands(&self.state.bands()) {
            Ok(()) => self.unsent = false,
            // Still unsent, so the next write interval retries.
            Err(e) => debug!("EQ write failed: {e}"),
        }
    }

    fn save(&self) {
        match self.state.save(&self.path) {
            Ok(()) => debug!("EQ saved to {}", self.path.display()),
            Err(e) => warn!("Could not save EQ to {}: {e}", self.path.display()),
        }
    }
}

// ---------------------------------------------------------------------------
// Mode transitions
// ---------------------------------------------------------------------------

fn enter_mode(mode: Mode, app: &RadioApp<RodioPlayer>, spotify: Option<&SpotifyHandle>) {
    match mode {
        // Both audible modes must un-pause first. The sink is shared, so a
        // sink the radio left paused consumes nothing — which silently
        // swallows Spotify's audio and blocks librespot's playback thread on
        // backpressure.
        Mode::Radio => {
            app.resume();
            app.play_current();
        }
        Mode::Streamer => {
            app.resume();
            if let Some(s) = spotify {
                s.set_enabled(true);
            }
        }
        Mode::Clock => {}
    }
}

fn leave_mode(mode: Mode, app: &RadioApp<RodioPlayer>, spotify: Option<&SpotifyHandle>) {
    match mode {
        Mode::Radio => {
            // Halt streaming and decoding so we are not pulling data for audio
            // nobody can hear.
            app.stop_playback();
        }
        Mode::Streamer => {
            // Deactivate + mute but keep the session alive so a reconnect
            // resumes cleanly.
            if let Some(s) = spotify {
                s.set_enabled(false);
            }
        }
        Mode::Clock => {}
    }
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

pub fn run(
    radios: RadioList,
    mono: bool,
    eq_file: PathBuf,
    handle: Handle,
    sink_tx: tokio::sync::oneshot::Sender<Arc<rodio::Sink>>,
    spotify_rx: Receiver<SpotifyHandle>,
    shutdown: Arc<AtomicBool>,
) {
    // Report readiness before the slow peripheral init: startup progress is
    // covered by watchdog pings from here on, not by TimeoutStartSec.
    let watchdog = Watchdog::from_env();
    watchdog.ready();

    let Some(player) = open_audio(mono, &handle, &shutdown, &watchdog) else {
        return;
    };
    if sink_tx.send(player.sink_handle()).is_err() {
        warn!("Nobody is listening for the audio sink; stopping the panel");
        return;
    }
    let mut app = RadioApp::new(radios, player);

    // The Spotify receiver is started by `main` once it has the sink. Waited
    // for in short slices so a shutdown during startup is not stuck here for
    // the full timeout — systemd escalates to SIGKILL if we overrun.
    let spotify = wait_for_spotify(&spotify_rx, &shutdown);

    let i2c = open_i2c(&shutdown, &watchdog);
    let mut display = Display::new(i2c.as_ref().and_then(init_vfd));
    let mut knobs = i2c.as_ref().map(Knobs::init);
    if knobs.as_ref().is_some_and(|k| !k.any_present()) {
        warn!("No rotary knobs answered; they will be retried in the background");
    }
    let mode_encoder = init_mode_encoder(&handle);
    let mut tone = Tone::init(&eq_file, Instant::now());

    let now = Instant::now();
    let mut mode = mode_encoder
        .as_ref()
        .map_or(Mode::Radio, |e| Mode::from_position(e.current()));
    info!("Startup mode: {mode:?}");

    let mut select_button = Button::new(now);
    let mut eq_button = Button::new(now);
    let mut prev_position: Option<i32> = None;
    let mut tune_at: Option<Instant> = None;
    // A short-lived message (volume or EQ) shown in place of the normal text.
    let mut overlay_until: Option<Instant> = None;
    let mut overlay_text = String::new();
    let mut station_name = app
        .current_station()
        .map_or_else(|| "NO STATION".to_string(), |s| s.name.clone());

    if let Some(s) = &spotify {
        s.set_enabled(mode == Mode::Streamer);
    }
    enter_mode(mode, &app, spotify.as_ref());

    // Keep telling systemd we are still ticking. This is the only mechanism
    // that catches a *hang* rather than a crash: if this loop stops running,
    // the pings stop and systemd restarts us.
    let mut next_ping = watchdog.interval().map(|i| Instant::now() + i);

    let mut next_tick = Instant::now();
    while !shutdown.load(Ordering::Relaxed) {
        next_tick += POLL_PERIOD;
        let now = Instant::now();
        if let Some(sleep) = next_tick.checked_duration_since(now) {
            std::thread::sleep(sleep);
        } else {
            // We fell behind; resync rather than spinning to catch up.
            next_tick = now;
        }
        let now = Instant::now();

        if let (Some(due), Some(interval)) = (next_ping, watchdog.interval()) {
            if now >= due {
                watchdog.ping();
                next_ping = Some(now + interval);
            }
        }

        // Re-init any knob that is currently missing.
        if let Some(k) = knobs.as_mut() {
            k.service(now);
        }
        if let Some(t) = tone.as_mut() {
            t.service(now);
        }

        // ---- Mode selection ------------------------------------------------
        if let Some(encoder) = mode_encoder.as_ref() {
            let new_mode = Mode::from_position(encoder.current());
            if new_mode != mode {
                info!("Mode: {mode:?} -> {new_mode:?}");
                leave_mode(mode, &app, spotify.as_ref());
                enter_mode(new_mode, &app, spotify.as_ref());
                mode = new_mode;
                overlay_until = None;
                tune_at = None;
                // A button held across the transition must not fire in the
                // mode it was not pressed in.
                select_button.resync(false, now);
                eq_button.resync(false, now);
                display.blank();
            }
        }

        // ---- Input ---------------------------------------------------------
        if matches!(mode, Mode::Radio | Mode::Streamer) {
            if let Some(delta) = knobs.as_mut().and_then(|k| k.volume_delta()) {
                if delta != 0 {
                    let volume = app.change_volume(delta as f32 * VOLUME_STEP);
                    info!("Volume: {volume:.2}");
                    overlay_text = volume_display_text(volume);
                    overlay_until = Some(now + OVERLAY);
                    // Reaching for the volume knob means you want to hear
                    // something. Radio mode only: in Streamer mode this
                    // would act on the radio's sink.
                    if mode == Mode::Radio {
                        app.resume();
                    }
                }
            }

            // A failed read simply isn't fed to the debouncer, so the last
            // believed level holds instead of producing a phantom edge.
            if let Some(raw) = knobs.as_mut().and_then(|k| k.select_button()) {
                if select_button.update(raw, now) == Some(Edge::Press) {
                    match mode {
                        Mode::Radio => {
                            let paused = app.toggle_playback();
                            info!("Playback {}", if paused { "paused" } else { "playing" });
                        }
                        Mode::Streamer => {
                            if let Some(s) = &spotify {
                                s.play_pause();
                                info!("Spotify play/pause toggled");
                            }
                        }
                        Mode::Clock => {}
                    }
                }
            }

            // EQ: a click moves to the next group, a turn adjusts the
            // selected one. Both show the group and its setting, so it is
            // always clear which group the knob is about to change.
            if let Some(delta) = knobs.as_mut().and_then(|k| k.eq_delta()) {
                if delta != 0 {
                    overlay_text = match tone.as_mut() {
                        Some(t) => {
                            let (group, offset) = t.adjust(delta, now);
                            info!("EQ {group:?}: {offset:+} dB");
                            eq_display_text(group, offset)
                        }
                        None => pad_display("NO EQ"),
                    };
                    overlay_until = Some(now + OVERLAY);
                }
            }
            if let Some(raw) = knobs.as_mut().and_then(|k| k.eq_button()) {
                if eq_button.update(raw, now) == Some(Edge::Press) {
                    overlay_text = match tone.as_mut() {
                        Some(t) => {
                            let (group, offset) = t.next_group();
                            info!("EQ group: {group:?}");
                            eq_display_text(group, offset)
                        }
                        None => pad_display("NO EQ"),
                    };
                    overlay_until = Some(now + OVERLAY);
                }
            }
        }

        if mode == Mode::Radio {
            if let Some(position) = knobs.as_mut().and_then(|k| k.select_position()) {
                match prev_position {
                    None => prev_position = Some(position),
                    Some(prev) if position != prev => {
                        // Apply the whole movement, not one step: the encoder
                        // reports an absolute position, so a fast turn between
                        // polls is a jump of several detents.
                        app.select_relative(position.wrapping_sub(prev));
                        prev_position = Some(position);
                        if let Some(station) = app.current_station() {
                            station_name = station.name.clone();
                            overlay_until = None;
                        }
                        tune_at = Some(now + STATION_SETTLE);
                    }
                    Some(_) => {}
                }
            }

            // Knob has settled — now actually tune in.
            if tune_at.is_some_and(|at| now >= at) {
                tune_at = None;
                info!("Tuning to: {station_name}");
                app.play_current();
            }
        }

        // ---- Display -------------------------------------------------------
        if overlay_until.is_some_and(|until| now >= until) {
            overlay_until = None;
        }
        let overlay = overlay_until.map(|_| overlay_text.clone());

        let text = match mode {
            Mode::Clock => clock_display_text(&chrono::Local::now()),
            // Just the station, not the connection state: on the panel that
            // reads as flicker. Silence already tells you it is not playing,
            // and if it stays silent the journal is where you find out why.
            Mode::Radio => overlay.unwrap_or_else(|| {
                if app.is_paused() {
                    pad_display("PAUSED")
                } else {
                    pad_display(&station_name)
                }
            }),
            Mode::Streamer => overlay.unwrap_or_else(|| {
                let status = spotify.as_ref().map(|s| s.status()).unwrap_or_default();
                pad_display(status.display())
            }),
        };
        display.show(&text);
    }

    info!("Panel stopping");
    watchdog.stopping();
    if let Some(t) = tone.as_mut() {
        t.flush();
    }
    app.stop_playback();
    display.blank();
}
