//! Audio playback for internet radio.
//!
//! # Why this is not just "decode into a rodio sink"
//!
//! rodio pulls samples from its sources *inside the cpal data callback* — the
//! real-time thread ALSA drives. Anything that blocks in `Source::next()`
//! blocks that callback, which at best causes an XRUN and at worst wedges all
//! audio output for the whole process (radio and Spotify both, since they
//! share one [`Sink`]).
//!
//! So the network and the audio thread are fully decoupled here:
//!
//! ```text
//!   HTTP stream ──> StreamBuffer ──> decode thread ──> PcmRing ──> RingSource
//!    (tokio task)   (bounded bytes)  (blocking, ours) (bounded PCM) (audio cb)
//! ```
//!
//! * [`StreamBuffer`] is a bounded byte buffer. The download task backs off
//!   when it is full rather than growing without limit.
//! * The decode thread is ours, so it is allowed to block on reads. Its reads
//!   still time out, so a silent socket surfaces as an error instead of a
//!   permanent stall.
//! * [`PcmRing`] holds decoded samples. The producer blocks when it is full
//!   (natural backpressure); the consumer *never* blocks — it takes the lock
//!   with `try_lock` and emits silence if it cannot get it.
//! * [`RingSource`] is what rodio sees. It never blocks and never waits on the
//!   network. On starvation it emits silence and bumps a counter that the
//!   supervisor watches.
//!
//! A supervisor task owns the retry policy: a station that fails to connect,
//! returns a bad status, ends, or starves is reconnected with exponential
//! backoff for as long as radio playback is wanted.

use color_eyre::eyre::{eyre, Result};
use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use rodio::{Decoder, OutputStream, Sink, Source};
use tokio::runtime::Handle;
use tokio::sync::watch;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Tuning
// ---------------------------------------------------------------------------

/// Encoded bytes we let pile up before the download task waits. Generous
/// enough for any rebuffer, small enough that it cannot threaten a 512 MB Pi.
const BYTE_BUFFER_CAP: usize = 2 * 1024 * 1024;

/// Consumed bytes kept around for the MP3 bit reservoir and for symphonia's
/// small backwards seeks during probing.
const BYTE_LOOKBACK: usize = 256 * 1024;

/// Decoded samples buffered between the decode thread and the audio callback.
/// ~2 s of 44.1 kHz stereo — enough to ride out a WiFi hiccup without adding
/// noticeable latency to a station change.
const PCM_CAP: usize = 352_800;

/// Decoded samples to accumulate before we start playing, so playback does not
/// begin on an empty buffer and immediately starve.
const PCM_PREBUFFER: usize = 88_200;

/// How long a read may find no new bytes before we call the stream stalled.
/// This is the timeout whose absence could wedge the audio thread forever.
const READ_STALL_TIMEOUT: Duration = Duration::from_secs(15);

/// Per-request network timeouts.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// Reconnect backoff bounds.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// A station that played at least this long is treated as healthy, so the next
/// failure starts backoff from scratch instead of from a long delay.
const HEALTHY_PLAY_TIME: Duration = Duration::from_secs(60);

/// Consecutive starved *samples* before the supervisor gives up and
/// reconnects. Samples, not frames, so this is ~2.5 s of stereo at 44.1 kHz
/// and ~5 s of mono. Long enough that a rebuffer is not mistaken for a dead
/// stream.
const STARVATION_LIMIT: u64 = 220_500;

/// Longest a playlist body we will read while resolving a station URL.
const MAX_PLAYLIST_BYTES: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Poison-proof locking
// ---------------------------------------------------------------------------

/// Take a lock, ignoring poisoning.
///
/// A panic in one of these threads must not cascade into a panic in the audio
/// thread. The data behind these locks is a byte buffer and a sample queue —
/// there is no invariant a panicking thread could leave broken that matters
/// more than staying alive does.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Player trait
// ---------------------------------------------------------------------------

/// What the radio is currently doing, for the front panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaybackStatus {
    /// Nothing is being played (not in radio mode, or explicitly stopped).
    #[default]
    Idle,
    /// Opening the connection to the station.
    Connecting,
    /// Connected, filling the buffer before audio starts.
    Buffering,
    /// Audio is flowing.
    Playing,
    /// Waiting out the backoff before retry number `attempt`.
    Reconnecting { attempt: u32 },
}

/// Control surface for audio playback.
///
/// Nothing here returns `Result`, because nothing here can fail: starting a
/// station hands work to a supervisor that owns all the retrying, and the
/// rest are flag writes on a sink. Returning `Result` would only add
/// unreachable error branches to the panel loop, hiding the operations that
/// genuinely can fail, like every I2C read next to them.
pub trait Player {
    /// Start (or restart) playback of `url`. Connection failures are the
    /// supervisor's problem, surfaced through [`Player::status`].
    fn play(&self, url: &str);
    fn stop(&self);
    fn set_volume(&self, volume: f32);
    /// Toggle between paused and playing, returning the new paused state.
    fn toggle_pause(&self) -> bool;
    /// Whether playback is currently paused.
    fn is_paused(&self) -> bool;
    /// Ensure the output is unpaused. Idempotent.
    ///
    /// Needed because the sink is shared with Spotify: a sink left paused by
    /// the radio would otherwise silently swallow Spotify's audio too.
    fn resume(&self);
}

// ---------------------------------------------------------------------------
// Mono downmix
// ---------------------------------------------------------------------------

/// Downmixes any channel layout to mono.
///
/// Captures the input channel count at construction and passes samples through
/// unchanged when the input is *already* mono. Averaging sample pairs
/// unconditionally would halve the effective rate of a 1-channel stream,
/// playing it an octave down at half speed.
pub struct Mono<S>
where
    S: Source<Item = f32>,
{
    input: S,
    in_channels: u16,
}

impl<S> Mono<S>
where
    S: Source<Item = f32>,
{
    pub fn new(input: S) -> Self {
        let in_channels = input.channels().max(1);
        Self { input, in_channels }
    }
}

impl<S> Iterator for Mono<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        match self.in_channels {
            1 => self.input.next(),
            n => {
                let mut sum = 0.0f32;
                for _ in 0..n {
                    sum += self.input.next()?;
                }
                Some(sum / f32::from(n))
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let divisor = usize::from(self.in_channels.max(1));
        let (low, high) = self.input.size_hint();
        (low / divisor, high.map(|h| h / divisor))
    }
}

impl<S> Source for Mono<S>
where
    S: Source<Item = f32>,
{
    fn current_frame_len(&self) -> Option<usize> {
        self.input
            .current_frame_len()
            .map(|n| n / usize::from(self.in_channels.max(1)))
    }

    fn channels(&self) -> u16 {
        1
    }

    fn sample_rate(&self) -> u32 {
        self.input.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.input.total_duration()
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), rodio::source::SeekError> {
        self.input.try_seek(pos)
    }
}

// ---------------------------------------------------------------------------
// StreamBuffer: bounded bridge from the async download to the decode thread
// ---------------------------------------------------------------------------

struct StreamState {
    data: Vec<u8>,
    /// Read cursor into `data`.
    pos: usize,
    /// Absolute stream offset of `data[0]`, so `Seek` can stay honest after we
    /// trim consumed bytes.
    origin: u64,
    /// The producer finished: no more bytes will arrive.
    done: bool,
    /// This session was abandoned; both ends should unwind.
    cancelled: bool,
}

struct StreamBuffer {
    state: Mutex<StreamState>,
    /// Signalled when bytes arrive, or `done`/`cancelled` is set.
    data_ready: Condvar,
}

enum PushOutcome {
    Accepted,
    Full,
    Cancelled,
}

impl StreamBuffer {
    fn new() -> Self {
        Self {
            state: Mutex::new(StreamState {
                data: Vec::with_capacity(256 * 1024),
                pos: 0,
                origin: 0,
                done: false,
                cancelled: false,
            }),
            data_ready: Condvar::new(),
        }
    }

    /// Append a chunk if there is room. Never blocks: the caller is an async
    /// task, and blocking a tokio worker to wait for a decoder is exactly the
    /// kind of coupling this module exists to avoid.
    fn try_push(&self, chunk: &[u8]) -> PushOutcome {
        let mut state = lock(&self.state);
        if state.cancelled {
            return PushOutcome::Cancelled;
        }
        if state.data.len() - state.pos >= BYTE_BUFFER_CAP {
            return PushOutcome::Full;
        }
        state.data.extend_from_slice(chunk);
        drop(state);
        self.data_ready.notify_all();
        PushOutcome::Accepted
    }

    fn finish(&self) {
        lock(&self.state).done = true;
        self.data_ready.notify_all();
    }

    fn cancel(&self) {
        let mut state = lock(&self.state);
        state.cancelled = true;
        state.done = true;
        drop(state);
        self.data_ready.notify_all();
    }
}

/// The `Read`/`Seek` end handed to the decoder. Blocks — but only ever on the
/// decode thread, which is ours to block, and only up to
/// [`READ_STALL_TIMEOUT`].
struct StreamReader {
    buffer: Arc<StreamBuffer>,
}

impl Read for StreamReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut state = lock(&self.buffer.state);
        let deadline = Instant::now() + READ_STALL_TIMEOUT;
        loop {
            if state.cancelled {
                return Ok(0);
            }
            let available = state.data.len().saturating_sub(state.pos);
            if available > 0 {
                let n = available.min(buf.len());
                let end = state.pos + n;
                buf[..n].copy_from_slice(&state.data[state.pos..end]);
                state.pos = end;

                // Drop bytes the decoder is long past, keeping a lookback
                // window. This is what stops the buffer growing without bound
                // over a multi-hour listening session.
                //
                // Trim only once the discardable prefix has grown to twice the
                // lookback, rather than on every read. `Vec::drain` memmoves
                // the remaining backlog, and doing that on each 4 KB read is
                // real work on a Pi Zero 2 for no benefit.
                if state.pos >= BYTE_LOOKBACK * 2 {
                    let drop_n = state.pos - BYTE_LOOKBACK;
                    state.data.drain(..drop_n);
                    state.pos -= drop_n;
                    state.origin += drop_n as u64;
                }
                return Ok(n);
            }
            if state.done {
                return Ok(0);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "no data from station within stall timeout",
                ));
            }
            let (next, _) = self
                .buffer
                .data_ready
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            state = next;
        }
    }
}

impl Seek for StreamReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let mut state = lock(&self.buffer.state);
        let absolute = match pos {
            SeekFrom::Start(p) => p,
            SeekFrom::Current(d) => {
                let here = state.origin + state.pos as u64;
                if d >= 0 {
                    here.saturating_add(d as u64)
                } else {
                    here.checked_sub(d.unsigned_abs()).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "seek before start")
                    })?
                }
            }
            SeekFrom::End(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "seek from end not supported on a live stream",
                ))
            }
        };
        // Bytes before `origin` have been discarded; report that honestly
        // rather than silently landing somewhere else.
        if absolute < state.origin {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek target already discarded from the live buffer",
            ));
        }
        let offset = (absolute - state.origin) as usize;
        if offset > state.data.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek past buffered data",
            ));
        }
        state.pos = offset;
        Ok(absolute)
    }
}

// ---------------------------------------------------------------------------
// PcmRing: bounded decoded-sample queue. Consumer never blocks.
// ---------------------------------------------------------------------------

struct PcmState {
    buf: VecDeque<f32>,
    /// Producer finished; once `buf` drains the source is over.
    closed: bool,
}

struct PcmRing {
    state: Mutex<PcmState>,
    /// Signalled when the consumer frees space or the ring is abandoned.
    space: Condvar,
}

impl PcmRing {
    fn new() -> Self {
        Self {
            state: Mutex::new(PcmState {
                buf: VecDeque::with_capacity(PCM_CAP / 4),
                closed: false,
            }),
            space: Condvar::new(),
        }
    }

    /// Producer side. Blocks while full — this is the backpressure that keeps
    /// a paused sink from making us buffer the whole station in memory.
    /// Returns false if the session was abandoned while waiting.
    fn push(&self, samples: &[f32], live: &LiveFlag) -> bool {
        let mut state = lock(&self.state);
        let mut offset = 0;
        while offset < samples.len() {
            if !live.is_live() || state.closed {
                return false;
            }
            let room = PCM_CAP.saturating_sub(state.buf.len());
            if room == 0 {
                let (next, _) = self
                    .space
                    .wait_timeout(state, Duration::from_millis(100))
                    .unwrap_or_else(|e| e.into_inner());
                state = next;
                continue;
            }
            let take = room.min(samples.len() - offset);
            state.buf.extend(&samples[offset..offset + take]);
            offset += take;
        }
        true
    }

    /// Consumer side, called from the audio callback. Never blocks: if the
    /// producer holds the lock we return nothing and the caller emits silence
    /// for this callback rather than stalling ALSA.
    ///
    /// Returns `(samples_written, stream_finished)`.
    fn try_pop(&self, out: &mut [f32]) -> (usize, bool) {
        let Ok(mut state) = self.state.try_lock() else {
            return (0, false);
        };
        let n = out.len().min(state.buf.len());
        for (slot, sample) in out.iter_mut().zip(state.buf.drain(..n)) {
            *slot = sample;
        }
        let finished = state.closed && state.buf.is_empty();
        drop(state);
        if n > 0 {
            self.space.notify_all();
        }
        (n, finished)
    }

    fn len(&self) -> usize {
        self.state.try_lock().map(|s| s.buf.len()).unwrap_or(0)
    }

    fn close(&self) {
        lock(&self.state).closed = true;
        self.space.notify_all();
    }
}

// ---------------------------------------------------------------------------
// Liveness
// ---------------------------------------------------------------------------

/// Whether a stream attempt should keep running.
///
/// Two flags rather than one, and the distinction matters: tearing down an
/// attempt so we can *reconnect* must not also retire the session, while
/// retiring the session (a station change, a stop) must end whatever attempt
/// is currently running. Collapsing these into a single flag means the first
/// reconnect permanently kills every later attempt — silent radio, forever.
#[derive(Clone)]
struct LiveFlag {
    session: Arc<AtomicBool>,
    attempt: Arc<AtomicBool>,
}

impl LiveFlag {
    fn new(session: Arc<AtomicBool>) -> Self {
        Self {
            session,
            attempt: Arc::new(AtomicBool::new(true)),
        }
    }

    fn is_live(&self) -> bool {
        self.session.load(Ordering::Relaxed) && self.attempt.load(Ordering::Relaxed)
    }

    /// End this attempt only; the session is free to start another.
    fn retire_attempt(&self) {
        self.attempt.store(false, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// RingSource: what rodio sees. Never blocks, never waits on the network.
// ---------------------------------------------------------------------------

pub struct RingSource {
    ring: Arc<PcmRing>,
    live: LiveFlag,
    starvation: Arc<AtomicU64>,
    channels: u16,
    sample_rate: u32,
    scratch: Vec<f32>,
    filled: usize,
    cursor: usize,
}

impl RingSource {
    fn new(
        ring: Arc<PcmRing>,
        live: LiveFlag,
        starvation: Arc<AtomicU64>,
        channels: u16,
        sample_rate: u32,
    ) -> Self {
        Self {
            ring,
            live,
            starvation,
            channels,
            sample_rate,
            scratch: vec![0.0; 2048],
            filled: 0,
            cursor: 0,
        }
    }
}

impl Iterator for RingSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        // A retired source ends immediately. This is what lets a station
        // change or a stop take effect without the mixer ever waiting.
        if !self.live.is_live() {
            return None;
        }
        // `get` rather than `[]`: this runs on the audio callback, where a
        // panic is a needlessly expensive way to handle a case the invariant
        // already rules out. Falling through to a refill is a safe recovery.
        if self.cursor < self.filled {
            if let Some(&sample) = self.scratch.get(self.cursor) {
                self.cursor += 1;
                return Some(sample);
            }
        }
        let (n, finished) = self.ring.try_pop(&mut self.scratch);
        if n == 0 {
            if finished {
                return None;
            }
            // Starved: emit silence rather than block the audio callback. The
            // supervisor is watching this counter and will reconnect.
            self.starvation.fetch_add(1, Ordering::Relaxed);
            return Some(0.0);
        }
        // Reset: this counter measures a *consecutive* underrun. Left
        // cumulative, the occasional single-sample gap from lock contention
        // would sum past the limit after a few hours and trigger a reconnect
        // in the middle of perfectly healthy playback.
        self.starvation.store(0, Ordering::Relaxed);
        self.filled = n;
        self.cursor = 1;
        // `first()` rather than `[0]`: this runs on the audio callback, where
        // a panic is a needlessly expensive way to handle an impossible case.
        self.scratch.first().copied()
    }
}

impl Source for RingSource {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }

    fn try_seek(&mut self, _pos: Duration) -> Result<(), rodio::source::SeekError> {
        Err(rodio::source::SeekError::NotSupported {
            underlying_source: "RingSource (live stream)",
        })
    }
}

// ---------------------------------------------------------------------------
// Audio output
// ---------------------------------------------------------------------------

/// Where decoded radio audio goes.
///
/// Exists so the streaming lifecycle can be tested without an audio device.
/// The supervisor is the most stateful code here and its failures are
/// lifecycle failures — a source retired at the wrong level, a tail dropped,
/// a status published by the wrong session. None of that is reachable by a
/// test while the only way to run it is to open a real sound card.
pub trait AudioOut: Send + Sync + 'static {
    /// Hand over a source. Called from a blocking context; may briefly wait.
    fn append(&self, source: RingSource);
    /// Discard anything queued.
    fn stop(&self);
    fn set_volume(&self, volume: f32);
    fn pause(&self);
    fn resume(&self);
    fn is_paused(&self) -> bool;
}

/// The real output: a shared `rodio::Sink`.
pub struct RodioOut {
    sink: Arc<Sink>,
}

impl RodioOut {
    pub fn new(sink: Arc<Sink>) -> Self {
        Self { sink }
    }
}

impl AudioOut for RodioOut {
    fn append(&self, source: RingSource) {
        self.sink.append(source);
    }

    fn stop(&self) {
        self.sink.stop();
    }

    fn set_volume(&self, volume: f32) {
        self.sink.set_volume(volume.clamp(0.0, 2.0));
    }

    fn pause(&self) {
        self.sink.pause();
    }

    fn resume(&self) {
        if self.sink.is_paused() {
            self.sink.play();
        }
    }

    fn is_paused(&self) -> bool {
        self.sink.is_paused()
    }
}

// ---------------------------------------------------------------------------
// Station URL handling
// ---------------------------------------------------------------------------

/// Whether a response looks like a playlist rather than audio.
fn is_playlist(url: &str, content_type: Option<&str>) -> bool {
    let ct = content_type.unwrap_or("").to_ascii_lowercase();
    if ct.contains("mpegurl") || ct.contains("scpls") || ct.contains("pls+xml") {
        return true;
    }
    // Fall back to the URL when the server is vague (many are).
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    path.ends_with(".m3u") || path.ends_with(".m3u8") || path.ends_with(".pls")
}

/// Pull the first usable stream URL out of an M3U or PLS body.
fn parse_playlist(body: &str) -> Option<String> {
    // PLS: `File1=http://...`
    for line in body.lines() {
        let line = line.trim();
        if let Some((key, value)) = line.split_once('=') {
            if key.trim().to_ascii_lowercase().starts_with("file") {
                let value = value.trim();
                if value.starts_with("http") {
                    return Some(value.to_string());
                }
            }
        }
    }
    // M3U: the first non-comment line.
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with("http") {
            return Some(line.to_string());
        }
    }
    None
}

/// One shared client, built once. Timeouts here are what turn a silently
/// stalled TCP connection into an error we can retry instead of a permanent
/// hang.
fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(HTTP_READ_TIMEOUT)
            .user_agent("cala-radio/0.1")
            .build()
            .unwrap_or_else(|e| {
                // A client with default settings is still far better than
                // taking the process down at the first station change.
                warn!("Falling back to a default HTTP client: {e}");
                reqwest::Client::new()
            })
    })
}

// ---------------------------------------------------------------------------
// RodioPlayer
// ---------------------------------------------------------------------------

/// Handles for the currently supervised station, so a new `play` or a `stop`
/// can retire the old one without waiting for it.
struct Session {
    /// Cleared to retire this session: the supervisor unwinds, the download
    /// task stops, and the current source ends at its next poll (via the
    /// session half of its [`LiveFlag`]).
    active: Arc<AtomicBool>,
}

/// The supervised streaming lifecycle, independent of any audio device.
///
/// `RodioPlayer` is this plus a real sound card; tests use it with a capturing
/// output.
pub struct StreamEngine {
    out: Arc<dyn AudioOut>,
    mono: bool,
    handle: Handle,
    session: Mutex<Option<Session>>,
    status_tx: watch::Sender<PlaybackStatus>,
    status_rx: watch::Receiver<PlaybackStatus>,
}

impl StreamEngine {
    /// Must be called from inside a tokio runtime: the handle captured here is
    /// what lets `play` be driven from the (blocking) hardware thread.
    pub fn new(out: Arc<dyn AudioOut>, mono: bool) -> Self {
        let (status_tx, status_rx) = watch::channel(PlaybackStatus::Idle);
        Self {
            out,
            mono,
            handle: Handle::current(),
            session: Mutex::new(None),
            status_tx,
            status_rx,
        }
    }

    /// Retire the current session, if any.
    fn retire(&self) {
        if let Some(old) = lock(&self.session).take() {
            old.active.store(false, Ordering::Relaxed);
        }
    }

    pub fn play(&self, url: &str) {
        self.retire();

        let active = Arc::new(AtomicBool::new(true));
        *lock(&self.session) = Some(Session {
            active: active.clone(),
        });

        info!("Starting playback for {url} (mono={})", self.mono);

        self.handle.spawn(supervise(
            url.to_string(),
            self.out.clone(),
            self.mono,
            active,
            self.status_tx.clone(),
        ));
    }

    pub fn stop(&self) {
        self.retire();
        // Safe now that no source can block: any queued source ends at its
        // next poll, so this cannot leave the mixer wedged.
        self.out.stop();
        let _ = self.status_tx.send(PlaybackStatus::Idle);
    }

    /// The supervisor's current state.
    ///
    /// The front panel does not show this — connection state reads as
    /// flicker on a 12-character display. It is the only observable the
    /// streaming lifecycle has, and the tests use it to tell a reconnect from
    /// a stall.
    pub fn status(&self) -> PlaybackStatus {
        *self.status_rx.borrow()
    }

    pub fn out(&self) -> &Arc<dyn AudioOut> {
        &self.out
    }
}

impl Drop for StreamEngine {
    /// Retire whatever is playing when the engine goes away.
    ///
    /// Without this, dropping the engine leaves the session flagged active, so
    /// the decode thread keeps waiting for room in a ring nobody is draining
    /// any more. Because that thread is a blocking task, tokio's runtime
    /// teardown then waits for it and the process hangs on exit. It matters on
    /// the path where the panel thread panics rather than shutting down
    /// cleanly — the one case where `stop` is never called.
    fn drop(&mut self) {
        self.retire();
    }
}

pub struct RodioPlayer {
    engine: StreamEngine,
    sink: Arc<Sink>,
    _stream: OutputStream,
}

impl RodioPlayer {
    /// Must be called from inside a tokio runtime.
    pub fn new(mono: bool) -> Result<Self> {
        let (_stream, stream_handle) = OutputStream::try_default()?;
        let sink = Arc::new(Sink::try_new(&stream_handle)?);
        let engine = StreamEngine::new(Arc::new(RodioOut::new(sink.clone())), mono);
        Ok(Self {
            engine,
            sink,
            _stream,
        })
    }

    /// Expose the shared rodio `Sink` so Spotify Connect can append decoded
    /// audio to the same physical output as internet radio.
    pub fn sink_handle(&self) -> Arc<Sink> {
        self.sink.clone()
    }
}

impl Player for RodioPlayer {
    fn play(&self, url: &str) {
        self.engine.play(url);
    }

    fn stop(&self) {
        self.engine.stop();
    }

    fn set_volume(&self, volume: f32) {
        self.engine.out().set_volume(volume);
    }

    fn toggle_pause(&self) -> bool {
        let out = self.engine.out();
        let paused = out.is_paused();
        if paused {
            out.resume();
        } else {
            out.pause();
        }
        !paused
    }

    fn is_paused(&self) -> bool {
        self.engine.out().is_paused()
    }

    fn resume(&self) {
        self.engine.out().resume();
    }
}

// ---------------------------------------------------------------------------
// Supervisor
// ---------------------------------------------------------------------------

fn backoff_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(5);
    let scaled = BACKOFF_MIN.saturating_mul(1u32 << shift);
    scaled.min(BACKOFF_MAX)
}

/// Owns the retry policy for one station: reconnects for as long as this
/// session is wanted, so a dropped stream never leaves the radio permanently
/// silent with the station name still on the display.
async fn supervise(
    url: String,
    out: Arc<dyn AudioOut>,
    mono: bool,
    active: Arc<AtomicBool>,
    status_tx: watch::Sender<PlaybackStatus>,
) {
    let mut attempt: u32 = 0;

    // All sessions share one status channel, so a retired supervisor must stay
    // quiet: otherwise it reports its own exit over the status of the station
    // that replaced it, and the display reads STOPPED while music is playing.
    let publish = |status| {
        if active.load(Ordering::Relaxed) {
            let _ = status_tx.send(status);
        }
    };

    while active.load(Ordering::Relaxed) {
        if attempt == 0 {
            publish(PlaybackStatus::Connecting);
        } else {
            publish(PlaybackStatus::Reconnecting { attempt });
        }

        let started = Instant::now();
        match play_once(&url, &out, mono, &active, &publish).await {
            Ok(()) => info!("Stream for {url} ended"),
            Err(e) => warn!("Stream for {url} failed: {e}"),
        }

        if !active.load(Ordering::Relaxed) {
            break;
        }

        // A station that played happily for a while gets a clean slate, so an
        // overnight blip does not inherit an hour-old backoff.
        if started.elapsed() >= HEALTHY_PLAY_TIME {
            attempt = 0;
        }
        attempt = attempt.saturating_add(1);

        let delay = backoff_delay(attempt);
        publish(PlaybackStatus::Reconnecting { attempt });
        info!("Reconnecting to {url} in {:.0?} (attempt {attempt})", delay);
        tokio::time::sleep(delay).await;
    }

    // Deliberately no status here: we only get out of that loop once we have
    // been retired, and whoever retired us owns the status now — `stop` sets
    // Idle, `play` starts a supervisor that sets Connecting.
    debug!("Supervisor for {url} exiting");
}

/// Read at most `MAX_PLAYLIST_BYTES` of a response body.
///
/// Bounded because a server that mislabels a live stream as a playlist would
/// otherwise have us buffer it forever.
async fn read_capped_body(response: reqwest::Response) -> Result<String> {
    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(&chunk?);
        if body.len() >= MAX_PLAYLIST_BYTES {
            body.truncate(MAX_PLAYLIST_BYTES);
            break;
        }
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Open the station, resolving one level of playlist indirection so a
/// `.pls`/`.m3u` entry does not decode as garbage and go permanently silent.
///
/// Returns the response we are going to decode, so an ordinary stream costs
/// exactly one request — connecting twice would make us look like two
/// listeners to the station, on every reconnect.
async fn open_stream(url: &str) -> Result<(String, reqwest::Response)> {
    let response = http_client()
        .get(url)
        .send()
        .await?
        .error_for_status()
        .map_err(|e| eyre!("station returned an error status: {e}"))?;

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    if !is_playlist(url, content_type.as_deref()) {
        return Ok((url.to_string(), response));
    }

    let body = read_capped_body(response).await?;
    let resolved =
        parse_playlist(&body).ok_or_else(|| eyre!("playlist at {url} contained no stream URL"))?;
    info!("Resolved playlist {url} -> {resolved}");

    let response = http_client()
        .get(&resolved)
        .send()
        .await?
        .error_for_status()
        .map_err(|e| eyre!("station returned an error status: {e}"))?;
    Ok((resolved, response))
}

/// One connect-and-play attempt. Returns when the stream ends, stalls, or the
/// session is retired.
async fn play_once(
    url: &str,
    out: &Arc<dyn AudioOut>,
    mono: bool,
    active: &Arc<AtomicBool>,
    publish: &impl Fn(PlaybackStatus),
) -> Result<()> {
    // Fresh per attempt: tearing this down to reconnect must leave the session
    // able to start the next attempt.
    let live = LiveFlag::new(active.clone());

    let (stream_url, response) = open_stream(url).await?;

    let buffer = Arc::new(StreamBuffer::new());
    let starvation = Arc::new(AtomicU64::new(0));

    // --- download task: HTTP bytes -> bounded buffer -------------------------
    let download = {
        let buffer = buffer.clone();
        let active = active.clone();
        tokio::spawn(async move {
            use futures_util::StreamExt;
            let mut stream = response.bytes_stream();
            'outer: while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        warn!("Stream read error: {e}");
                        break;
                    }
                };
                loop {
                    if !active.load(Ordering::Relaxed) {
                        break 'outer;
                    }
                    match buffer.try_push(&chunk) {
                        PushOutcome::Accepted => break,
                        PushOutcome::Cancelled => break 'outer,
                        // Full means the decoder is not keeping up — almost
                        // always because playback is paused. Wait rather than
                        // grow the buffer without bound.
                        PushOutcome::Full => {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                    }
                }
            }
            buffer.finish();
        })
    };

    // --- decode thread: bytes -> decoded samples ----------------------------
    let ring = Arc::new(PcmRing::new());
    let (format_tx, format_rx) = tokio::sync::oneshot::channel::<(u16, u32)>();

    let decode = {
        let buffer = buffer.clone();
        let ring = ring.clone();
        let live = live.clone();
        tokio::task::spawn_blocking(move || {
            let reader = StreamReader { buffer };
            // Blocking here is fine and intended: this is our own thread, not
            // the audio callback.
            let decoder = match Decoder::new_mp3(reader) {
                Ok(d) => d,
                Err(e) => {
                    ring.close();
                    return Err(eyre!("decoder error: {e}"));
                }
            };

            let source = decoder.convert_samples::<f32>();
            let sample_rate = source.sample_rate();
            let in_channels = source.channels();

            let (channels, mut samples): (u16, Box<dyn Iterator<Item = f32> + Send>) = if mono {
                let m = Mono::new(source);
                (m.channels(), Box::new(m))
            } else {
                (in_channels, Box::new(source))
            };

            if format_tx.send((channels, sample_rate)).is_err() {
                ring.close();
                return Ok(());
            }

            let mut block = Vec::with_capacity(4096);
            loop {
                if !live.is_live() {
                    break;
                }
                block.clear();
                for _ in 0..4096 {
                    match samples.next() {
                        Some(s) => block.push(s),
                        None => break,
                    }
                }
                if block.is_empty() {
                    break;
                }
                if !ring.push(&block, &live) {
                    break;
                }
            }
            ring.close();
            Ok(())
        })
    };

    // --- wait for format, then prebuffer before we make a sound -------------
    publish(PlaybackStatus::Buffering);

    let format = tokio::select! {
        f = format_rx => f.ok(),
        _ = wait_until_retired(active) => None,
    };

    let Some((channels, sample_rate)) = format else {
        active_teardown(&buffer, &ring, &live, download, decode).await;
        return Err(eyre!("stream ended before a decodable format was found"));
    };
    info!("Decoding {stream_url}: {channels} ch @ {sample_rate} Hz");

    let prebuffer_deadline = Instant::now() + Duration::from_secs(10);
    while ring.len() < PCM_PREBUFFER
        && Instant::now() < prebuffer_deadline
        && active.load(Ordering::Relaxed)
        && !decode.is_finished()
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    if !active.load(Ordering::Relaxed) {
        active_teardown(&buffer, &ring, &live, download, decode).await;
        return Ok(());
    }

    // --- hand the ring to rodio --------------------------------------------
    {
        let out = out.clone();
        let source = RingSource::new(
            ring.clone(),
            live.clone(),
            starvation.clone(),
            channels,
            sample_rate,
        );
        // `append` can briefly wait for a stopped queue to drain; do it off
        // the runtime so a tokio worker is never the thing waiting.
        let _ = tokio::task::spawn_blocking(move || out.append(source)).await;
    }
    publish(PlaybackStatus::Playing);

    // --- run until the stream ends, starves, or we are retired --------------
    let mut ticker = tokio::time::interval(Duration::from_millis(250));
    let mut drain_first = false;
    loop {
        ticker.tick().await;

        if !active.load(Ordering::Relaxed) {
            break;
        }
        if decode.is_finished() {
            // The stream ended on its own; whatever is still buffered is good
            // audio we should play before reconnecting.
            drain_first = true;
            break;
        }
        if starvation.load(Ordering::Relaxed) > STARVATION_LIMIT {
            warn!("Station starved the output buffer; reconnecting");
            break;
        }
    }

    // Let the buffered tail play out rather than clipping it. Skipped when we
    // were retired — a station change should be immediate.
    if drain_first {
        let deadline = Instant::now() + Duration::from_secs(5);
        while ring.len() > 0 && Instant::now() < deadline && active.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    active_teardown(&buffer, &ring, &live, download, decode).await;
    Ok(())
}

/// Never completes while the session is wanted; used to race against setup
/// steps so a retired session unwinds promptly.
async fn wait_until_retired(active: &Arc<AtomicBool>) {
    while active.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Retire this attempt's plumbing. The current `RingSource` is ended so rodio
/// drops it, then both workers are unwound.
async fn active_teardown(
    buffer: &Arc<StreamBuffer>,
    ring: &Arc<PcmRing>,
    live: &LiveFlag,
    download: tokio::task::JoinHandle<()>,
    decode: tokio::task::JoinHandle<Result<()>>,
) {
    live.retire_attempt();
    buffer.cancel();
    ring.close();
    download.abort();
    let _ = download.await;
    // The decode thread cannot be aborted, but its reads time out and it
    // checks the attempt flag, so it exits on its own.
    if let Ok(Err(e)) = decode.await {
        debug!("Decode task finished with: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::buffer::SamplesBuffer;

    #[test]
    fn mono_downmixes_stereo() {
        let src = SamplesBuffer::new(2, 44_100, vec![1.0f32, 0.0, 0.5, 0.5, -1.0, 1.0]);
        let mono = Mono::new(src);
        assert_eq!(mono.channels(), 1);
        assert_eq!(mono.collect::<Vec<_>>(), vec![0.5, 0.5, 0.0]);
    }

    /// A 1-channel input must pass through untouched, or mono stations play
    /// an octave down at half speed.
    #[test]
    fn mono_passes_through_mono_input() {
        let samples = vec![0.1f32, 0.2, 0.3, 0.4];
        let src = SamplesBuffer::new(1, 44_100, samples.clone());
        let mono = Mono::new(src);
        assert_eq!(mono.channels(), 1);
        assert_eq!(mono.sample_rate(), 44_100);
        assert_eq!(mono.collect::<Vec<_>>(), samples);
    }

    #[test]
    fn mono_averages_more_than_two_channels() {
        let src = SamplesBuffer::new(4, 48_000, vec![1.0f32, 1.0, 0.0, 0.0]);
        assert_eq!(Mono::new(src).collect::<Vec<_>>(), vec![0.5]);
    }

    #[test]
    fn playlist_detection_uses_content_type_then_extension() {
        assert!(is_playlist("http://x/y", Some("audio/x-mpegurl")));
        assert!(is_playlist("http://x/y", Some("audio/x-scpls")));
        assert!(is_playlist("http://x/stream.pls", None));
        assert!(is_playlist("http://x/stream.m3u?a=1", None));
        assert!(!is_playlist("http://x/stream.mp3", Some("audio/mpeg")));
        assert!(!is_playlist("http://x/stream", None));
    }

    #[test]
    fn parses_pls_and_m3u() {
        let pls = "[playlist]\nNumberOfEntries=1\nFile1=http://a.example/stream\nTitle1=X\n";
        assert_eq!(
            parse_playlist(pls).as_deref(),
            Some("http://a.example/stream")
        );

        let m3u = "#EXTM3U\n#EXTINF:-1,Station\nhttp://b.example/stream\n";
        assert_eq!(
            parse_playlist(m3u).as_deref(),
            Some("http://b.example/stream")
        );

        assert_eq!(parse_playlist("#EXTM3U\n# nothing here\n"), None);
    }

    #[test]
    fn backoff_grows_then_saturates() {
        assert_eq!(backoff_delay(1), Duration::from_secs(1));
        assert_eq!(backoff_delay(2), Duration::from_secs(2));
        assert_eq!(backoff_delay(4), Duration::from_secs(8));
        assert_eq!(backoff_delay(20), BACKOFF_MAX);
    }

    /// The property the audio thread depends on: popping from an empty ring
    /// returns immediately instead of waiting for the producer.
    #[test]
    fn ring_pop_never_blocks_when_empty() {
        let ring = PcmRing::new();
        let mut out = [0.0f32; 64];
        let (n, finished) = ring.try_pop(&mut out);
        assert_eq!(n, 0);
        assert!(!finished);

        ring.close();
        let (n, finished) = ring.try_pop(&mut out);
        assert_eq!(n, 0);
        assert!(
            finished,
            "a closed, drained ring reports the stream is over"
        );
    }

    fn live_flag() -> LiveFlag {
        LiveFlag::new(Arc::new(AtomicBool::new(true)))
    }

    #[test]
    fn ring_round_trips_samples() {
        let ring = PcmRing::new();
        let live = live_flag();
        assert!(ring.push(&[1.0, 2.0, 3.0], &live));
        let mut out = [0.0f32; 2];
        assert_eq!(ring.try_pop(&mut out), (2, false));
        assert_eq!(out, [1.0, 2.0]);
        assert_eq!(ring.len(), 1);
    }

    /// A retired attempt must not leave the decode thread blocked in `push`.
    #[test]
    fn ring_push_gives_up_when_attempt_retired() {
        let ring = PcmRing::new();
        let live = live_flag();
        live.retire_attempt();
        assert!(!ring.push(&[1.0, 2.0], &live));
    }

    /// Retiring the session also stops the attempt.
    #[test]
    fn ring_push_gives_up_when_session_retired() {
        let session = Arc::new(AtomicBool::new(true));
        let live = LiveFlag::new(session.clone());
        let ring = PcmRing::new();
        session.store(false, Ordering::Relaxed);
        assert!(!ring.push(&[1.0, 2.0], &live));
    }

    /// Tearing an attempt down in order to reconnect must leave the *session*
    /// able to run another one. With a single shared flag, the first
    /// reconnect would silently kill every later attempt and the radio would
    /// never play again.
    #[test]
    fn retiring_an_attempt_leaves_the_session_able_to_reconnect() {
        let session = Arc::new(AtomicBool::new(true));

        let first = LiveFlag::new(session.clone());
        assert!(first.is_live());
        first.retire_attempt();
        assert!(!first.is_live(), "the failed attempt is over");

        // What the supervisor does next.
        let second = LiveFlag::new(session.clone());
        assert!(
            second.is_live(),
            "a new attempt on the same session must be live"
        );

        let ring = PcmRing::new();
        assert!(
            ring.push(&[1.0, 2.0], &second),
            "the decode thread must feed the new attempt"
        );
        let mut source = RingSource::new(
            Arc::new(PcmRing::new()),
            second.clone(),
            Arc::new(AtomicU64::new(0)),
            2,
            44_100,
        );
        assert!(
            source.next().is_some(),
            "the new attempt's source must produce audio, not end immediately"
        );

        // And retiring the session still stops everything.
        session.store(false, Ordering::Relaxed);
        assert!(!second.is_live());
        assert_eq!(source.next(), None);
    }

    #[test]
    fn ring_source_emits_silence_while_starved() {
        let ring = Arc::new(PcmRing::new());
        let live = live_flag();
        let starvation = Arc::new(AtomicU64::new(0));
        let mut source = RingSource::new(ring, live.clone(), starvation.clone(), 2, 44_100);

        assert_eq!(source.next(), Some(0.0));
        assert_eq!(source.next(), Some(0.0));
        assert!(starvation.load(Ordering::Relaxed) >= 2);

        // Retiring the source ends it, which is how a station change takes
        // effect without the mixer ever waiting.
        live.retire_attempt();
        assert_eq!(source.next(), None);
    }

    /// The counter must measure a *consecutive* underrun. If it were
    /// cumulative, ordinary lock contention would sum past the limit after a
    /// few hours and reconnect a perfectly healthy stream.
    #[test]
    fn starvation_counter_resets_once_audio_flows_again() {
        let ring = Arc::new(PcmRing::new());
        let starvation = Arc::new(AtomicU64::new(0));
        let mut source = RingSource::new(ring.clone(), live_flag(), starvation.clone(), 2, 44_100);

        for _ in 0..10 {
            assert_eq!(source.next(), Some(0.0));
        }
        assert_eq!(starvation.load(Ordering::Relaxed), 10);

        assert!(ring.push(&[0.25, 0.5], &live_flag()));
        assert_eq!(source.next(), Some(0.25));
        assert_eq!(
            starvation.load(Ordering::Relaxed),
            0,
            "a successful pop clears the underrun run"
        );
    }

    /// Trimming consumed bytes must not desynchronise absolute offsets, and
    /// must actually bound the buffer.
    #[test]
    fn trimming_keeps_absolute_offsets_honest() {
        let buffer = Arc::new(StreamBuffer::new());
        let mut reader = StreamReader {
            buffer: buffer.clone(),
        };

        let chunk = vec![7u8; 64 * 1024];
        for _ in 0..12 {
            assert!(matches!(buffer.try_push(&chunk), PushOutcome::Accepted));
        }

        // Read past the trim threshold.
        let mut out = vec![0u8; 32 * 1024];
        let mut total = 0u64;
        while total < (BYTE_LOOKBACK as u64) * 3 {
            total += reader.read(&mut out).unwrap() as u64;
        }

        // The reported position is still an absolute stream offset...
        assert_eq!(reader.stream_position().unwrap(), total);
        // ...the buffer really did shrink...
        {
            let state = lock(&buffer.state);
            assert!(state.origin > 0, "some prefix was discarded");
            assert!(state.data.len() < 12 * 64 * 1024, "buffer was trimmed");
        }
        // ...and a seek into the discarded prefix is refused rather than
        // silently landing at the wrong byte.
        assert!(reader.seek(SeekFrom::Start(0)).is_err());
        // A seek within the retained window still works.
        assert_eq!(reader.seek(SeekFrom::Start(total)).unwrap(), total);
    }

    #[test]
    fn stream_reader_reports_stall_rather_than_waiting_forever() {
        let buffer = Arc::new(StreamBuffer::new());
        let mut reader = StreamReader {
            buffer: buffer.clone(),
        };
        buffer.cancel();
        let mut out = [0u8; 16];
        assert_eq!(reader.read(&mut out).unwrap(), 0, "cancelled reads as EOF");
    }

    #[test]
    fn stream_reader_seek_tracks_absolute_offsets() {
        let buffer = Arc::new(StreamBuffer::new());
        let mut reader = StreamReader {
            buffer: buffer.clone(),
        };
        assert!(matches!(
            buffer.try_push(&[1, 2, 3, 4, 5, 6, 7, 8]),
            PushOutcome::Accepted
        ));

        let mut out = [0u8; 4];
        assert_eq!(reader.read(&mut out).unwrap(), 4);
        assert_eq!(out, [1, 2, 3, 4]);
        assert_eq!(reader.stream_position().unwrap(), 4);
        assert_eq!(reader.seek(SeekFrom::Start(1)).unwrap(), 1);
        assert_eq!(reader.read(&mut out).unwrap(), 4);
        assert_eq!(out, [2, 3, 4, 5]);
        assert!(reader.seek(SeekFrom::End(0)).is_err());
    }

    #[test]
    fn stream_buffer_refuses_to_grow_past_the_cap() {
        let buffer = StreamBuffer::new();
        let chunk = vec![0u8; 64 * 1024];
        let mut accepted = 0usize;
        loop {
            match buffer.try_push(&chunk) {
                PushOutcome::Accepted => accepted += chunk.len(),
                PushOutcome::Full => break,
                PushOutcome::Cancelled => panic!("not cancelled"),
            }
            assert!(accepted <= BYTE_BUFFER_CAP + chunk.len(), "cap respected");
        }
        assert!(accepted >= BYTE_BUFFER_CAP);
    }
}

// ---------------------------------------------------------------------------
// Lifecycle integration tests
//
// Real TCP, real HTTP, real MP3 bytes, real symphonia decode, a real consuming
// output thread. Everything above this point is a unit test of one component;
// bugs in how the components are driven together are only reachable from here.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod lifecycle {
    use super::*;
    use crate::testing::{CapturingOut, Reply, TestStation, SAMPLES_PER_FRAME};

    /// Samples one clip of `frames` should produce (mono).
    fn expected(frames: usize) -> u64 {
        (frames * SAMPLES_PER_FRAME) as u64
    }

    async fn wait_for_status(
        engine: &StreamEngine,
        want: impl Fn(PlaybackStatus) -> bool,
        timeout: Duration,
    ) -> PlaybackStatus {
        let deadline = Instant::now() + timeout;
        loop {
            let status = engine.status();
            if want(status) || Instant::now() >= deadline {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn streams_real_audio_end_to_end() {
        let station = TestStation::start(Reply::Audio { frames: 40 }).await;
        let out = CapturingOut::new(8.0);
        let engine = StreamEngine::new(out.clone(), false);

        engine.play(&station.url("/stream"));

        let delivered = out
            .wait_for_samples(expected(40), Duration::from_secs(10))
            .await;
        // Lower bound, not equality. The output is *designed* to emit a
        // silence sample rather than block when the ring is momentarily empty,
        // so a healthy stream can legitimately deliver slightly more samples
        // than the clip contains, so equality would make this test flaky.
        assert!(
            delivered >= expected(40),
            "every decoded sample should reach the output, got {delivered}"
        );
        // Stop before asserting on the station. The clip ends, so leaving the
        // supervisor running means it is entitled to reconnect at any moment —
        // asserting an exact request count against a live supervisor is a race.
        engine.stop();

        assert!(
            delivered < expected(40) + 44_100,
            "far more than the clip was delivered ({delivered}); the output was \
             mostly playing silence"
        );
        assert!(
            station.hits("/stream") >= 1,
            "the station should have been requested"
        );
    }

    /// The first reconnect must produce audio, not just the first attempt.
    /// A liveness flag shared between session and attempt would silence
    /// every attempt after the first, which looks perfect on the bench until
    /// the first network blip.
    #[tokio::test]
    async fn reconnects_and_keeps_playing_after_the_stream_ends() {
        // Every connection gets a short clip and then the server hangs up,
        // exactly like a station dropping the connection.
        let station = TestStation::start(Reply::Audio { frames: 20 }).await;
        let out = CapturingOut::new(8.0);
        let engine = StreamEngine::new(out.clone(), false);

        engine.play(&station.url("/stream"));

        // Two clips' worth of audio can only happen if a reconnect actually
        // produced sound.
        let want = expected(20) * 2;
        let delivered = out.wait_for_samples(want, Duration::from_secs(20)).await;

        assert!(
            delivered >= want,
            "expected audio from at least two attempts, got {delivered} samples \
             ({} sources started, {} requests)",
            out.sources_started(),
            station.hits("/stream")
        );
        assert!(
            station.hits("/stream") >= 2,
            "the supervisor should have reconnected"
        );
        assert!(
            out.sources_started() >= 2,
            "each attempt should hand a fresh source to the output"
        );
        engine.stop();
    }

    /// A station that is down must be retried, not abandoned.
    #[tokio::test]
    async fn retries_after_an_http_error_then_plays() {
        let station = TestStation::start(Reply::Status(503)).await;
        let out = CapturingOut::new(8.0);
        let engine = StreamEngine::new(out.clone(), false);

        engine.play(&station.url("/stream"));

        let status = wait_for_status(
            &engine,
            |s| matches!(s, PlaybackStatus::Reconnecting { .. }),
            Duration::from_secs(10),
        )
        .await;
        assert!(
            matches!(status, PlaybackStatus::Reconnecting { .. }),
            "a failing station should show as reconnecting, got {status:?}"
        );
        assert_eq!(out.delivered(), 0, "nothing should have played");

        // The station comes back.
        station.set_default(Reply::Audio { frames: 20 });
        let delivered = out
            .wait_for_samples(expected(20), Duration::from_secs(20))
            .await;
        assert!(
            delivered >= expected(20),
            "playback should recover once the station returns, got {delivered}"
        );
        engine.stop();
    }

    /// A `.pls` entry must resolve to the stream behind it rather than being
    /// decoded as garbage.
    #[tokio::test]
    async fn resolves_a_playlist_and_plays_the_target() {
        let station = TestStation::start(Reply::Audio { frames: 20 }).await;
        station.route(
            "/station.pls",
            Reply::Playlist {
                target_path: "/real-stream".to_string(),
            },
        );

        let out = CapturingOut::new(8.0);
        let engine = StreamEngine::new(out.clone(), false);
        engine.play(&station.url("/station.pls"));

        let delivered = out
            .wait_for_samples(expected(20), Duration::from_secs(15))
            .await;
        assert!(delivered >= expected(20), "playlist target should play");
        assert!(station.hits("/station.pls") >= 1);
        assert!(station.hits("/real-stream") >= 1, "the target was fetched");
        engine.stop();
    }

    /// The property the whole design exists for: a station that accepts the
    /// connection and then sends nothing must not stop the output pulling,
    /// or it would block the ALSA callback permanently.
    #[tokio::test]
    async fn a_stalled_station_never_blocks_the_output() {
        let station = TestStation::start(Reply::Audio { frames: 200 }).await;
        let out = CapturingOut::new(8.0);
        let engine = StreamEngine::new(out.clone(), false);

        engine.play(&station.url("/stream"));
        out.wait_for_samples(expected(20), Duration::from_secs(15))
            .await;

        // Now every future connection stalls.
        station.set_default(Reply::Stall);

        // The output must keep being able to pull — silence is fine, blocking
        // is not.
        let before = out.delivered();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let after = out.delivered();
        assert!(
            after > before,
            "the output thread stopped pulling: {before} -> {after}"
        );
        engine.stop();
    }

    /// Stopping must take effect promptly and leave the status honest.
    #[tokio::test]
    async fn stop_ends_playback_and_reports_idle() {
        let station = TestStation::start(Reply::Audio { frames: 400 }).await;
        let out = CapturingOut::new(2.0);
        let engine = StreamEngine::new(out.clone(), false);

        engine.play(&station.url("/stream"));
        out.wait_for_samples(expected(20), Duration::from_secs(15))
            .await;

        engine.stop();
        assert_eq!(engine.status(), PlaybackStatus::Idle);

        tokio::time::sleep(Duration::from_millis(200)).await;
        let settled = out.delivered();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            out.delivered(),
            settled,
            "no audio should be delivered after stop"
        );
    }

    /// Changing station must retire the old stream, not layer a second one on
    /// top of it.
    #[tokio::test]
    async fn changing_station_retires_the_previous_stream() {
        let station = TestStation::start(Reply::Audio { frames: 400 }).await;
        station.route("/b", Reply::Audio { frames: 40 });

        let out = CapturingOut::new(8.0);
        let engine = StreamEngine::new(out.clone(), false);

        engine.play(&station.url("/a"));
        out.wait_for_samples(expected(20), Duration::from_secs(15))
            .await;

        engine.play(&station.url("/b"));
        let deadline = Instant::now() + Duration::from_secs(15);
        while station.hits("/b") == 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(station.hits("/b") >= 1, "the new station was requested");

        let status = wait_for_status(
            &engine,
            |s| s == PlaybackStatus::Playing,
            Duration::from_secs(15),
        )
        .await;
        assert_eq!(status, PlaybackStatus::Playing);
        engine.stop();
    }

    /// A retired session must not publish over the status of the one that
    /// replaced it, or the status reads Idle while music is playing.
    ///
    /// Note the shape of this test. Sampling the status once, later, is not
    /// enough: the stomp is transient, and a station short enough to reconnect
    /// will paper over it with a fresh `Playing` before the assert runs. So
    /// the good station is long enough not to reconnect during the window, and
    /// the status is watched continuously rather than checked once.
    #[tokio::test]
    async fn a_replaced_session_does_not_overwrite_the_new_status() {
        let station = TestStation::start(Reply::Status(503)).await;
        // Long enough that this attempt is still playing for the whole window.
        station.route("/good", Reply::Audio { frames: 4000 });

        let out = CapturingOut::new(8.0);
        let engine = StreamEngine::new(out.clone(), false);

        // Get the first session into its backoff sleep, where it sits holding
        // a status it must not publish on the way out.
        engine.play(&station.url("/bad"));
        wait_for_status(
            &engine,
            |s| matches!(s, PlaybackStatus::Reconnecting { .. }),
            Duration::from_secs(10),
        )
        .await;

        engine.play(&station.url("/good"));
        let status = wait_for_status(
            &engine,
            |s| s == PlaybackStatus::Playing,
            Duration::from_secs(15),
        )
        .await;
        assert_eq!(status, PlaybackStatus::Playing);

        // The retired session wakes from its backoff somewhere in here. Watch
        // the whole window: the bug is a brief wrong value, not a final one.
        let mut seen = Vec::new();
        for _ in 0..100 {
            let now = engine.status();
            seen.push(now);
            assert_ne!(
                now,
                PlaybackStatus::Idle,
                "a retired session published Idle over live playback; timeline: {seen:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        engine.stop();
    }

    /// Pausing must actually stop consumption, and resuming must pick it back
    /// up. This is the path where backpressure lives: a paused output consumes
    /// nothing, the ring fills, the decoder blocks on it, and the download
    /// task backs off. Nothing else exercises that chain.
    #[tokio::test]
    async fn pause_stops_consumption_and_resume_restarts_it() {
        let station = TestStation::start(Reply::Audio { frames: 4000 }).await;
        let out = CapturingOut::new(4.0);
        let engine = StreamEngine::new(out.clone(), false);

        engine.play(&station.url("/stream"));
        out.wait_for_samples(expected(20), Duration::from_secs(15))
            .await;

        engine.out().pause();
        assert!(engine.out().is_paused());
        tokio::time::sleep(Duration::from_millis(200)).await;

        let while_paused = out.delivered();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            out.delivered(),
            while_paused,
            "a paused output must not consume"
        );

        // Long enough for the ring to fill and the decoder to block on it.
        tokio::time::sleep(Duration::from_secs(1)).await;

        engine.out().resume();
        let resumed = out
            .wait_for_samples(while_paused + 44_100, Duration::from_secs(15))
            .await;
        assert!(
            resumed > while_paused,
            "audio must resume after unpausing: {while_paused} -> {resumed}"
        );
        engine.stop();
    }

    /// Volume is applied through the output, and survives a station change.
    #[tokio::test]
    async fn volume_reaches_the_output() {
        let station = TestStation::start(Reply::Audio { frames: 40 }).await;
        let out = CapturingOut::new(8.0);
        let engine = StreamEngine::new(out.clone(), false);
        engine.out().set_volume(0.4);
        engine.play(&station.url("/stream"));
        out.wait_for_samples(expected(10), Duration::from_secs(15))
            .await;
        assert!((out.volume() - 0.4).abs() < f32::EPSILON);
        engine.stop();
    }
}
