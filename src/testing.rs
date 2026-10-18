//! Test support: a fake radio station, and an audio output that records what
//! it was given.
//!
//! Real TCP, real HTTP, real MP3 bytes, real symphonia decode. The bugs this
//! exists to catch are lifecycle bugs — an attempt retired at the wrong level,
//! a buffered tail dropped, a status published by a session that no longer
//! owns it — and none of them are visible from a unit test of an isolated
//! component. They only appear when a stream actually fails and the supervisor
//! has to do something about it.
//!
//! The MP3 is generated rather than shipped as a fixture: a frame whose side
//! info is all zeros has `part2_3_length == 0` for every granule, so it
//! carries no Huffman data and decodes to 1152 samples of silence. That makes
//! streams of any length reproducible from code, with no binary in the repo.
//!
//! Silence is fine for these tests because what is being asserted is *how many
//! samples arrived and when*, not what they sounded like. A pipeline that
//! never ran delivers nothing; one that starved reports starvation.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::player::{AudioOut, RingSource};
use rodio::Source;

/// Bytes in one MPEG-1 Layer III frame at 44.1 kHz, 64 kbps: `144 * 64000 /
/// 44100`, truncated.
const FRAME_LEN: usize = 208;

/// Decoded samples per MPEG-1 Layer III frame.
pub const SAMPLES_PER_FRAME: usize = 1152;

/// Build `frames` worth of valid, decodable MPEG-1 Layer III mono audio.
pub fn silent_mp3(frames: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(frames * FRAME_LEN);
    for _ in 0..frames {
        let mut frame = vec![0u8; FRAME_LEN];
        frame[0] = 0xFF; // sync
        frame[1] = 0xFB; // sync + MPEG-1 + Layer III + no CRC
        frame[2] = 0x50; // 64 kbps, 44.1 kHz, no padding
        frame[3] = 0xC4; // mono
        data.extend_from_slice(&frame);
    }
    data
}

/// How the station answers a request.
#[derive(Clone, Debug)]
pub enum Reply {
    /// Serve MP3 audio, then close the connection.
    Audio { frames: usize },
    /// Serve MP3 audio in chunks with a gap between them.
    SlowAudio { frames: usize, gap: Duration },
    /// Send headers and then nothing at all, holding the connection open.
    Stall,
    /// Answer with an HTTP error status.
    Status(u16),
    /// Answer with a PLS playlist pointing at another path on this station.
    Playlist { target_path: String },
}

struct State {
    routes: HashMap<String, Reply>,
    default: Reply,
}

/// A station listening on localhost.
pub struct TestStation {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
    hits: Arc<Mutex<HashMap<String, u32>>>,
    connections: Arc<AtomicU32>,
}

impl TestStation {
    /// Start listening. The server task lives as long as the process; tests
    /// are short and this keeps the type simple to use.
    pub async fn start(default: Reply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let addr = listener.local_addr().expect("local addr");

        let state = Arc::new(Mutex::new(State {
            routes: HashMap::new(),
            default,
        }));
        let hits: Arc<Mutex<HashMap<String, u32>>> = Arc::new(Mutex::new(HashMap::new()));
        let connections = Arc::new(AtomicU32::new(0));

        let station = Self {
            addr,
            state: state.clone(),
            hits: hits.clone(),
            connections: connections.clone(),
        };

        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                connections.fetch_add(1, Ordering::Relaxed);
                let state = state.clone();
                let hits = hits.clone();
                tokio::spawn(async move {
                    let _ = serve(stream, state, hits).await;
                });
            }
        });

        station
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    /// Give one path its own behaviour.
    pub fn route(&self, path: &str, reply: Reply) {
        self.state
            .lock()
            .unwrap()
            .routes
            .insert(path.to_string(), reply);
    }

    /// Change what every unrouted path answers with, from now on.
    pub fn set_default(&self, reply: Reply) {
        self.state.lock().unwrap().default = reply;
    }

    /// How many requests this path has received.
    pub fn hits(&self, path: &str) -> u32 {
        self.hits.lock().unwrap().get(path).copied().unwrap_or(0)
    }

    /// How many TCP connections have been accepted in total.
    pub fn connections(&self) -> u32 {
        self.connections.load(Ordering::Relaxed)
    }
}

async fn serve(
    mut stream: tokio::net::TcpStream,
    state: Arc<Mutex<State>>,
    hits: Arc<Mutex<HashMap<String, u32>>>,
) -> std::io::Result<()> {
    // Read just enough to find the request line.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 16 * 1024 {
            break;
        }
    }

    let request = String::from_utf8_lossy(&buf);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();

    *hits.lock().unwrap().entry(path.clone()).or_insert(0) += 1;

    let reply = {
        let state = state.lock().unwrap();
        state
            .routes
            .get(&path)
            .cloned()
            .unwrap_or_else(|| state.default.clone())
    };

    match reply {
        Reply::Audio { frames } => {
            let body = silent_mp3(frames);
            stream
                .write_all(audio_headers(body.len()).as_bytes())
                .await?;
            stream.write_all(&body).await?;
            stream.flush().await?;
            // Closing is what an ended stream looks like to the client.
        }
        Reply::SlowAudio { frames, gap } => {
            stream
                .write_all(audio_headers_streaming().as_bytes())
                .await?;
            for _ in 0..frames {
                stream.write_all(&silent_mp3(1)).await?;
                stream.flush().await?;
                tokio::time::sleep(gap).await;
            }
        }
        Reply::Stall => {
            stream
                .write_all(audio_headers_streaming().as_bytes())
                .await?;
            stream.flush().await?;
            // Hold the connection open, sending nothing: the failure that
            // must never be able to wedge the audio callback.
            tokio::time::sleep(Duration::from_secs(120)).await;
        }
        Reply::Status(code) => {
            let body = format!("error {code}");
            let response = format!(
                "HTTP/1.1 {code} Error\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await?;
        }
        Reply::Playlist { target_path } => {
            let local = stream.local_addr()?;
            let body = format!(
                "[playlist]\r\nNumberOfEntries=1\r\nFile1=http://{local}{target_path}\r\nTitle1=Test\r\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: audio/x-scpls\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await?;
        }
    }
    Ok(())
}

fn audio_headers(len: usize) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n"
    )
}

fn audio_headers_streaming() -> String {
    // No Content-Length: an endless stream, exactly like a real Icecast mount.
    "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nConnection: close\r\n\r\n".to_string()
}

// ---------------------------------------------------------------------------
// Capturing audio output
// ---------------------------------------------------------------------------

/// An [`AudioOut`] that pulls from its sources like a sound card would and
/// records what it received.
///
/// It pulls on a real thread at (a multiple of) real time rather than as fast
/// as it can, because that is what the pipeline is designed around: pulling
/// flat out would never exercise buffering, and never pulling would never
/// exercise backpressure.
pub struct CapturingOut {
    state: Arc<Mutex<CapState>>,
    paused: Arc<std::sync::atomic::AtomicBool>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    volume: Arc<Mutex<f32>>,
}

#[derive(Default)]
struct CapState {
    queue: std::collections::VecDeque<RingSource>,
    /// Samples actually pulled from sources.
    delivered: u64,
    /// How many sources have been started.
    sources_started: u32,
    stop_requested: bool,
}

impl CapturingOut {
    /// `speed` multiplies real time: 4.0 drains four seconds of audio per
    /// second of wall clock, which keeps tests quick without letting the
    /// consumer outrun a healthy producer.
    pub fn new(speed: f64) -> Arc<Self> {
        let state = Arc::new(Mutex::new(CapState::default()));
        let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let out = Arc::new(Self {
            state: state.clone(),
            paused: paused.clone(),
            shutdown: shutdown.clone(),
            volume: Arc::new(Mutex::new(1.0)),
        });

        std::thread::Builder::new()
            .name("capturing-out".into())
            .spawn(move || pump(state, paused, shutdown, speed))
            .expect("spawn the capturing output thread");

        out
    }

    /// Total samples pulled from all sources so far.
    pub fn delivered(&self) -> u64 {
        self.state.lock().unwrap().delivered
    }

    /// How many sources have been started (one per successful attempt).
    pub fn sources_started(&self) -> u32 {
        self.state.lock().unwrap().sources_started
    }

    pub fn volume(&self) -> f32 {
        *self.volume.lock().unwrap()
    }

    /// Wait until `delivered` reaches `target`, or give up.
    pub async fn wait_for_samples(&self, target: u64, timeout: Duration) -> u64 {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let n = self.delivered();
            if n >= target || std::time::Instant::now() >= deadline {
                return n;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Drop for CapturingOut {
    fn drop(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

fn pump(
    state: Arc<Mutex<CapState>>,
    paused: Arc<std::sync::atomic::AtomicBool>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    speed: f64,
) {
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    let mut current: Option<RingSource> = None;
    let mut next_due = Instant::now();

    while !shutdown.load(Ordering::Relaxed) {
        if paused.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(2));
            next_due = Instant::now();
            continue;
        }

        {
            let mut st = state.lock().unwrap();
            if st.stop_requested {
                st.stop_requested = false;
                st.queue.clear();
                current = None;
            }
            if current.is_none() {
                if let Some(source) = st.queue.pop_front() {
                    st.sources_started += 1;
                    current = Some(source);
                    next_due = Instant::now();
                }
            }
        }

        let Some(source) = current.as_mut() else {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        };

        let rate = f64::from(source.sample_rate()) * f64::from(source.channels()) * speed;
        let mut got = 0u64;
        for _ in 0..1024 {
            match source.next() {
                Some(_) => got += 1,
                None => {
                    current = None;
                    break;
                }
            }
        }
        if got > 0 {
            state.lock().unwrap().delivered += got;
            next_due += Duration::from_secs_f64(got as f64 / rate);
            let now = Instant::now();
            if let Some(wait) = next_due.checked_duration_since(now) {
                std::thread::sleep(wait);
            } else {
                next_due = now;
            }
        } else if current.is_none() {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

impl AudioOut for CapturingOut {
    fn append(&self, source: RingSource) {
        self.state.lock().unwrap().queue.push_back(source);
    }

    fn stop(&self) {
        let mut st = self.state.lock().unwrap();
        st.stop_requested = true;
        st.queue.clear();
    }

    fn set_volume(&self, volume: f32) {
        *self.volume.lock().unwrap() = volume;
    }

    fn pause(&self) {
        self.paused
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn resume(&self) {
        self.paused
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    fn is_paused(&self) -> bool {
        self.paused.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_frames_have_the_expected_shape() {
        let data = silent_mp3(3);
        assert_eq!(data.len(), 3 * FRAME_LEN);
        for frame in data.chunks(FRAME_LEN) {
            assert_eq!(frame[0], 0xFF, "frame sync");
            assert_eq!(frame[1] & 0xFE, 0xFA, "MPEG-1 Layer III");
        }
    }
}
