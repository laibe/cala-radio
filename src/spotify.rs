//! Spotify Connect receiver integration via librespot.
//!
//! The receiver runs for the whole application lifetime (across mode
//! switches). In Streamer mode audio is routed to the shared rodio sink; in
//! Clock/Radio mode the session stays alive but is deactivated and muted, so a
//! reconnect to the "same" device keeps its context and resumes cleanly
//! (avoiding librespot's reconnect-context bug and "dealer channel closed"
//! errors from tearing down the session).
//!
//! Everything here is supervised. A librespot session ends for entirely
//! ordinary reasons — the dealer disconnects, a token expires, WiFi
//! reconnects, the mDNS responder restarts — and none of those should leave
//! Streamer mode permanently dead until someone power-cycles the radio.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use color_eyre::eyre::{eyre, Result};
use futures_util::StreamExt;
use librespot::connect::{ConnectConfig, Spirc};
use librespot::core::authentication::Credentials;
use librespot::core::cache::Cache;
use librespot::core::config::{DeviceType, SessionConfig};
use librespot::core::session::Session;
use librespot::discovery::Discovery;
use librespot::playback::audio_backend::{Sink, SinkError, SinkResult};
use librespot::playback::config::PlayerConfig;
use librespot::playback::convert::Converter;
use librespot::playback::decoder::AudioPacket;
use librespot::playback::mixer::{self, MixerConfig};
use librespot::playback::player::{Player, PlayerEvent, PlayerEventChannel};
use tracing::{debug, error, info, warn};

/// Name shown to Spotify clients for this device.
pub const DEVICE_NAME: &str = "Cala Radio";

/// On-disk cache location for credentials + (optionally) decoded audio.
pub const CACHE_DIR: &str = ".cache";

/// Sample rate / channel layout librespot decodes to.
const SAMPLE_RATE: u32 = 44100;
const NUM_CHANNELS: u16 = 2;

/// How many decoded packets we let queue in the shared sink before applying
/// backpressure. Kept shallow so pause/stop reacts promptly and so a no-op
/// `stop()` leaves only a very short tail of audio behind.
const MAX_QUEUED_SOURCES: usize = 16;

/// How long the sink may fail to drain before we drop a packet instead of
/// waiting. Without this bound a paused sink blocks librespot's playback
/// thread forever, which is unrecoverable from the front panel.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

/// Backoff bounds for rebuilding a dead session or a dead discovery service.
const RETRY_MIN: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Spotify status surfaced to the hardware (VFD + buttons).
// ---------------------------------------------------------------------------

/// Snapshot of the Spotify Connect receiver state, polled by the main loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpotifyStatus {
    /// A Spotify session is active (a device is linked and playing/queued).
    pub linked: bool,
    /// Whether the current track is playing (vs paused).
    pub playing: bool,
}

impl SpotifyStatus {
    /// Short label suitable for the 12-char VFD.
    pub fn display(&self) -> &'static str {
        if !self.linked {
            "STREAM"
        } else if self.playing {
            "SPOTIFY"
        } else {
            "PAUSED"
        }
    }
}

// ---------------------------------------------------------------------------
// Switchable audio sink: routes to the shared rodio output when enabled.
// ---------------------------------------------------------------------------

/// A librespot [`Sink`] that appends decoded PCM to the shared `rodio::Sink`
/// while `enabled` is true, and drops the samples otherwise. The sink is
/// shared with the librespot `Player`, so toggling `enabled` mutes/unmutes
/// Spotify output without tearing down the session.
pub struct SwitchableSink {
    sink: Arc<rodio::Sink>,
    enabled: Arc<AtomicBool>,
}

impl SwitchableSink {
    pub fn new(sink: Arc<rodio::Sink>, enabled: Arc<AtomicBool>) -> Self {
        Self { sink, enabled }
    }

    fn write_samples(&self, samples: &[f32]) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }

        // Throttle so we don't over-buffer faster than the hardware plays.
        // Bounded, and re-checking `enabled`, because the sink may simply never
        // drain: it is shared with the radio, and a sink the radio left paused
        // consumes nothing at all. Blocking here indefinitely would wedge
        // librespot's playback thread with no way out from the knobs.
        let deadline = Instant::now() + DRAIN_TIMEOUT;
        while self.sink.len() > MAX_QUEUED_SOURCES {
            if !self.enabled.load(Ordering::Relaxed) {
                return;
            }
            if Instant::now() >= deadline {
                debug!("Shared sink is not draining; dropping a Spotify packet");
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        let source = rodio::buffer::SamplesBuffer::new(NUM_CHANNELS, SAMPLE_RATE, samples.to_vec());
        self.sink.append(source);
    }
}

impl Sink for SwitchableSink {
    fn start(&mut self) -> SinkResult<()> {
        Ok(())
    }

    fn stop(&mut self) -> SinkResult<()> {
        // Deliberately does *not* touch the shared sink.
        //
        // librespot calls this on every track end and pause. Forwarding it to
        // `rodio::Sink::stop()` set the `stopped` flag on the sink the radio
        // also plays through, which silences radio audio and arms rodio's
        // blocking `append` path for the next station start. The queue we
        // leave behind is at most MAX_QUEUED_SOURCES deep, so the audible tail
        // is a fraction of a second.
        Ok(())
    }

    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        match packet {
            AudioPacket::Samples(samples) => {
                let samples_f32 = converter.f64_to_f32(&samples);
                self.write_samples(&samples_f32);
                Ok(())
            }
            AudioPacket::Raw(_) => Err(SinkError::OnWrite(
                "raw packets unsupported in SwitchableSink".into(),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Background driver.
// ---------------------------------------------------------------------------

enum Command {
    PlayPause,
    SetEnabled(bool),
    Shutdown,
}

/// Everything belonging to one live librespot session.
///
/// Kept as three separate fields rather than a struct because the select loop
/// below needs to borrow the task handle and the event receiver in different
/// branches at the same time.
type Link = (Spirc, tokio::task::JoinHandle<()>, PlayerEventChannel);

struct Inner {
    cmd_rx: tokio::sync::mpsc::UnboundedReceiver<Command>,
    status_tx: tokio::sync::watch::Sender<SpotifyStatus>,
    rodio_sink: Arc<rodio::Sink>,
    enabled: Arc<AtomicBool>,
}

fn retry_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(5);
    RETRY_MIN.saturating_mul(1u32 << shift).min(RETRY_MAX)
}

impl Inner {
    async fn run(mut self) {
        let device_id = device_id(DEVICE_NAME);
        let session_config = SessionConfig {
            device_id: device_id.clone(),
            ..SessionConfig::default()
        };

        let cache: Option<Cache> =
            match Cache::new(Some(CACHE_DIR), Some(CACHE_DIR), Some(CACHE_DIR), None) {
                Ok(c) => Some(c),
                Err(e) => {
                    // A cache-less receiver still works; it just re-authenticates
                    // via zeroconf after a restart. Far better than giving up.
                    warn!("Spotify cache unavailable, continuing without it: {e}");
                    None
                }
            };

        let mut discovery = launch_discovery(&device_id, &session_config);
        let mut discovery_retry: u32 = 0;
        let mut discovery_at: Option<Instant> = if discovery.is_none() {
            // Even the very first launch can fail (mDNS not up yet at boot);
            // retry instead of never being discoverable.
            Some(Instant::now() + retry_delay(1))
        } else {
            None
        };

        let mut spirc: Option<Spirc> = None;
        let mut link_task: Option<tokio::task::JoinHandle<()>> = None;
        let mut link_events: Option<PlayerEventChannel> = None;
        // Last credentials that produced a working session, so a dropped
        // session can be rebuilt without waiting for the phone to re-link.
        let mut credentials: Option<Credentials> = cache.as_ref().and_then(|c| c.credentials());
        let mut rebuild_at: Option<Instant> = None;
        let mut rebuild_attempt: u32 = 0;

        loop {
            tokio::select! {
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(Command::PlayPause) => {
                            if let Some(sp) = &spirc {
                                if let Err(e) = sp.play_pause() {
                                    warn!("Spotify play/pause failed: {e}");
                                }
                            } else {
                                debug!("Play/pause ignored: no Spotify session linked");
                            }
                        }
                        Some(Command::SetEnabled(enabled)) => {
                            self.enabled.store(enabled, Ordering::Relaxed);
                            if let Some(sp) = &spirc {
                                let result = if enabled {
                                    sp.activate()
                                } else {
                                    // Pause (don't disconnect) so the session's
                                    // context/queue stays intact and a later
                                    // re-activate resumes seamlessly. The sink is
                                    // muted by the flag, so no audio is heard.
                                    sp.pause()
                                };
                                if let Err(e) = result {
                                    warn!("Spotify {} failed: {e}", if enabled { "activate" } else { "pause" });
                                }
                            }
                            if !enabled {
                                self.publish(|s| s.playing = false);
                            }
                        }
                        Some(Command::Shutdown) | None => break,
                    }
                }

                creds = async {
                    match discovery.as_mut() {
                        Some(d) => d.next().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match creds {
                        Some(new_credentials) => {
                            discovery_retry = 0;
                            info!("Spotify device linked by {:?}", new_credentials.username);
                            credentials = Some(new_credentials.clone());
                            if spirc.is_none() {
                                rebuild_at = None;
                                rebuild_attempt = 0;
                                match self.try_build(&session_config, &cache, new_credentials).await {
                                    Some((sp, task, events)) => {
                                        spirc = Some(sp);
                                        link_task = Some(task);
                                        link_events = Some(events);
                                    }
                                    None => {
                                        rebuild_at = Some(Instant::now() + retry_delay(1));
                                        rebuild_attempt = 1;
                                    }
                                }
                            }
                            // If a session already exists, ignore a new credential
                            // event for the same device.
                        }
                        None => {
                            // The mDNS responder went away. Relaunch it rather
                            // than silently never being discoverable again.
                            //
                            // Scheduled rather than slept inline: awaiting here
                            // would stop the whole loop servicing commands, so
                            // the Select button would do nothing for up to a
                            // minute while we waited to retry.
                            warn!("Spotify discovery stream ended; will relaunch");
                            // Drop the old responder so the mDNS socket is free.
                            drop(discovery.take());
                            discovery_retry = discovery_retry.saturating_add(1);
                            discovery_at = Some(Instant::now() + retry_delay(discovery_retry));
                        }
                    }
                }

                // A session that dies must be noticed, otherwise it sits in
                // place looking alive and Streamer mode stays broken until
                // reboot.
                _ = async {
                    match link_task.as_mut() {
                        Some(task) => {
                            let _ = task.await;
                        }
                        None => std::future::pending().await,
                    }
                } => {
                    warn!("Spotify session ended; scheduling a rebuild");
                    spirc = None;
                    link_task = None;
                    link_events = None;
                    self.publish(|s| { s.linked = false; s.playing = false; });
                    rebuild_attempt = rebuild_attempt.saturating_add(1);
                    rebuild_at = Some(Instant::now() + retry_delay(rebuild_attempt));
                }

                event = async {
                    match link_events.as_mut() {
                        Some(events) => events.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match event {
                        Some(event) => self.on_player_event(event),
                        None => {
                            // Must clear the receiver, not just log. A closed
                            // channel returns `None` immediately and forever,
                            // so leaving it in place turns this select branch
                            // into a 100%-CPU spin. The session teardown is
                            // handled by the link_task branch.
                            debug!("Spotify player event channel closed");
                            link_events = None;
                        }
                    }
                }

                _ = async {
                    match discovery_at {
                        Some(at) => tokio::time::sleep_until(at.into()).await,
                        None => std::future::pending().await,
                    }
                } => {
                    discovery_at = None;
                    discovery = launch_discovery(&device_id, &session_config);
                    if discovery.is_none() {
                        discovery_retry = discovery_retry.saturating_add(1);
                        discovery_at = Some(Instant::now() + retry_delay(discovery_retry));
                    }
                }

                _ = async {
                    match rebuild_at {
                        Some(at) => tokio::time::sleep_until(at.into()).await,
                        None => std::future::pending().await,
                    }
                } => {
                    rebuild_at = None;
                    let creds = credentials.clone().or_else(|| cache.as_ref().and_then(|c| c.credentials()));
                    match creds {
                        Some(c) => {
                            info!("Rebuilding Spotify session (attempt {rebuild_attempt})");
                            match self.try_build(&session_config, &cache, c).await {
                                Some((sp, task, events)) => {
                                    spirc = Some(sp);
                                    link_task = Some(task);
                                    link_events = Some(events);
                                    rebuild_attempt = 0;
                                }
                                None => {
                                    rebuild_attempt = rebuild_attempt.saturating_add(1);
                                    rebuild_at = Some(Instant::now() + retry_delay(rebuild_attempt));
                                }
                            }
                        }
                        None => {
                            debug!("No cached Spotify credentials; waiting for a device to link");
                        }
                    }
                }
            }
        }

        if let Some(sp) = spirc.take() {
            if let Err(e) = sp.shutdown() {
                debug!("Spotify shutdown returned: {e}");
            }
        }
        if let Some(task) = link_task.take() {
            task.abort();
            let _ = task.await;
        }
        let _ = self.status_tx.send_replace(SpotifyStatus::default());
        info!("Spotify Connect receiver stopped");
    }

    fn publish(&self, edit: impl FnOnce(&mut SpotifyStatus)) {
        let mut status = *self.status_tx.borrow();
        edit(&mut status);
        let _ = self.status_tx.send_replace(status);
    }

    /// Translate librespot's player events into the two bits the front panel
    /// cares about.
    fn on_player_event(&self, event: PlayerEvent) {
        match event {
            PlayerEvent::Playing { .. } => self.publish(|s| {
                s.linked = true;
                s.playing = true;
            }),
            PlayerEvent::Paused { .. } => self.publish(|s| {
                s.linked = true;
                s.playing = false;
            }),
            PlayerEvent::Stopped { .. } | PlayerEvent::EndOfTrack { .. } => {
                self.publish(|s| s.playing = false)
            }
            PlayerEvent::SessionConnected { user_name, .. } => {
                info!("Spotify session connected for {user_name}");
                self.publish(|s| s.linked = true);
            }
            PlayerEvent::SessionDisconnected { .. } => self.publish(|s| {
                s.linked = false;
                s.playing = false;
            }),
            PlayerEvent::Unavailable { track_id, .. } => {
                warn!("Spotify track unavailable: {track_id:?}");
            }
            _ => {}
        }
    }

    async fn try_build(
        &self,
        session_config: &SessionConfig,
        cache: &Option<Cache>,
        credentials: Credentials,
    ) -> Option<Link> {
        match Self::build_spirc(
            session_config.clone(),
            cache.clone(),
            credentials,
            self.rodio_sink.clone(),
            self.enabled.clone(),
        )
        .await
        {
            Ok((spirc, task, events)) => {
                self.publish(|s| s.linked = true);
                Some((spirc, tokio::spawn(task), events))
            }
            Err(e) => {
                warn!("Spotify session failed: {e}");
                self.publish(|s| {
                    s.linked = false;
                    s.playing = false;
                });
                None
            }
        }
    }

    async fn build_spirc(
        session_config: SessionConfig,
        cache: Option<Cache>,
        credentials: Credentials,
        rodio_sink: Arc<rodio::Sink>,
        enabled: Arc<AtomicBool>,
    ) -> Result<(
        Spirc,
        impl std::future::Future<Output = ()>,
        PlayerEventChannel,
    )> {
        let session = Session::new(session_config, cache);
        let player_config = PlayerConfig::default();
        let mixer_config = MixerConfig::default();

        let mixer =
            mixer::find(None).ok_or_else(|| eyre!("no audio mixer available"))?(mixer_config)?;

        // The rodio sink (driven by the physical knob) is the single
        // master volume. Pin librespot's software mixer to full/unity so the
        // connect path passes audio through unattenuated, and disable Spotify's
        // remote volume control entirely (hides the in-app slider and stops
        // volume pushes to Spotify's servers).
        //
        // NOTE: Spirc::new sets the mixer to ConnectConfig::initial_volume, so
        // we must set that to full here — setting the mixer before Spirc::new
        // would be overwritten by the default ~50%.
        let start_enabled = enabled.load(Ordering::Relaxed);
        let sink_enabled = enabled.clone();
        let player = Player::new(
            player_config,
            session.clone(),
            mixer.get_soft_volume(),
            move || {
                Box::new(SwitchableSink::new(
                    rodio_sink.clone(),
                    sink_enabled.clone(),
                )) as Box<dyn librespot::playback::audio_backend::Sink>
            },
        );
        let events = player.get_player_event_channel();

        let connect_config = ConnectConfig {
            name: DEVICE_NAME.to_string(),
            device_type: DeviceType::Speaker,
            initial_volume: u16::MAX,
            disable_volume: true,
            volume_steps: 64,
            ..ConnectConfig::default()
        };

        let (spirc, task) = Spirc::new(
            connect_config,
            session.clone(),
            credentials,
            player.clone(),
            mixer,
        )
        .await?;

        // Activate only if currently in Streamer mode; otherwise leave it
        // deactivated (muted via the switchable sink) until mode switches back.
        if start_enabled {
            spirc.activate()?;
        } else if let Err(e) = spirc.pause() {
            debug!("Initial Spotify pause returned: {e}");
        }

        Ok((spirc, task, events))
    }
}

/// Advertise this device on the LAN. Returns `None` if the responder could not
/// be started; the caller retries rather than giving up for good.
fn launch_discovery(
    device_id: &str,
    session_config: &SessionConfig,
) -> Option<std::pin::Pin<Box<Discovery>>> {
    match Discovery::builder(device_id.to_string(), session_config.client_id.clone())
        .name(DEVICE_NAME)
        .launch()
    {
        Ok(d) => {
            info!("Spotify Connect receiver '{DEVICE_NAME}' ready on the LAN");
            Some(Box::pin(d))
        }
        Err(e) => {
            error!("Failed to start Spotify discovery: {e}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Public manager handle.
// ---------------------------------------------------------------------------

/// Cheap, `Send + Clone` control surface for the receiver.
///
/// The front-panel loop runs on its own (blocking) thread, so it needs a
/// handle it can own without taking the task's `JoinHandle` with it.
#[derive(Clone)]
pub struct SpotifyHandle {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<Command>,
    status_rx: tokio::sync::watch::Receiver<SpotifyStatus>,
    enabled: Arc<AtomicBool>,
}

impl SpotifyHandle {
    /// Current receiver status (polled by the panel loop for the VFD).
    pub fn status(&self) -> SpotifyStatus {
        *self.status_rx.borrow()
    }

    /// Toggle Spotify play/pause (used by the physical Select button).
    pub fn play_pause(&self) {
        let _ = self.cmd_tx.send(Command::PlayPause);
    }

    /// Activate (Streamer mode) or deactivate + mute (Clock/Radio mode).
    /// The session stays alive either way so a reconnect resumes cleanly.
    pub fn set_enabled(&self, enabled: bool) {
        // Set the flag first: it is what releases the sink's backpressure loop,
        // and the command is handled asynchronously.
        self.enabled.store(enabled, Ordering::Relaxed);
        let _ = self.cmd_tx.send(Command::SetEnabled(enabled));
    }
}

/// Public handle to the Spotify Connect receiver background task.
pub struct Spotify {
    handle: SpotifyHandle,
    task: tokio::task::JoinHandle<()>,
}

impl Spotify {
    /// Start the receiver, feeding audio into the shared `rodio::Sink`.
    pub fn start(rodio_sink: Arc<rodio::Sink>) -> Self {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (status_tx, status_rx) = tokio::sync::watch::channel(SpotifyStatus::default());
        let enabled = Arc::new(AtomicBool::new(true));

        let inner = Inner {
            cmd_rx,
            status_tx,
            rodio_sink,
            enabled: enabled.clone(),
        };
        let task = tokio::spawn(inner.run());

        Self {
            handle: SpotifyHandle {
                cmd_tx,
                status_rx,
                enabled,
            },
            task,
        }
    }

    /// A cloneable control surface for other threads.
    pub fn handle(&self) -> SpotifyHandle {
        self.handle.clone()
    }

    /// Stop the receiver and wait for the background task to finish.
    ///
    /// Bounded: a receiver that will not wind up must not hold up a clean
    /// service stop, because systemd escalates to SIGKILL if we dawdle.
    pub async fn shutdown(self) {
        self.handle.enabled.store(false, Ordering::Relaxed);
        let _ = self.handle.cmd_tx.send(Command::Shutdown);
        if tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .is_err()
        {
            warn!("Spotify receiver did not stop within 5s; abandoning it");
        }
    }
}

/// librespot derives the device id from a SHA-1 of the device name (matching
/// the discovery `device_id` so the zeroconf handshake lines up).
fn device_id(name: &str) -> String {
    use sha1::{Digest, Sha1};
    hex::encode(Sha1::digest(name.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_display() {
        assert_eq!(SpotifyStatus::default().display(), "STREAM");
        let playing = SpotifyStatus {
            linked: true,
            playing: true,
        };
        assert_eq!(playing.display(), "SPOTIFY");
        let paused = SpotifyStatus {
            linked: true,
            playing: false,
        };
        assert_eq!(paused.display(), "PAUSED");
    }

    #[test]
    fn status_display_fits_the_vfd() {
        for status in [
            SpotifyStatus::default(),
            SpotifyStatus {
                linked: true,
                playing: true,
            },
            SpotifyStatus {
                linked: true,
                playing: false,
            },
        ] {
            assert!(status.display().len() <= 12, "{}", status.display());
        }
    }

    #[test]
    fn device_id_is_hex_sha1() {
        let id = device_id(DEVICE_NAME);
        assert_eq!(id.len(), 40);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(id, device_id(DEVICE_NAME));
    }

    #[test]
    fn retry_delay_grows_then_saturates() {
        assert_eq!(retry_delay(1), RETRY_MIN);
        assert_eq!(retry_delay(2), Duration::from_secs(4));
        assert_eq!(retry_delay(30), RETRY_MAX);
    }
}
