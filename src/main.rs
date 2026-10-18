//! Cala Radio — an always-on internet radio / clock / Spotify Connect
//! receiver for a Raspberry Pi Zero 2 W.
//!
//! # Structure
//!
//! * `main` runs on the tokio runtime: logging, signals, and the Spotify
//!   receiver live here.
//! * The front panel (I2C display, rotary knobs, mode selector) is driven by a
//!   dedicated OS thread. I2C transactions block, and the audio stream lives
//!   behind a cpal handle that is not `Send`, so both belong on a thread of
//!   their own rather than on a runtime worker.
//!
//! # Failure policy
//!
//! This runs unattended in a piece of furniture. Nothing about a peripheral is
//! allowed to be fatal: a missing display, a knob that will not answer, or a
//! mode selector whose GPIO lines are still held by a previous run all
//! degrade to "keep playing music" and are retried in the background. The one
//! thing worth exiting for is a total failure to open an audio device, and
//! even that is retried for a minute first — after which systemd restarts us
//! (see `supervisor.nu`).

#![warn(clippy::unwrap_used, clippy::expect_used)]

use clap::Parser;
use color_eyre::eyre::Result;
use radio::radio_list::RadioList;
use tracing::{error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[cfg(target_os = "linux")]
mod panel;

/// Width of the vacuum fluorescent display, in characters.
pub const DISPLAY_WIDTH: usize = 12;

#[derive(Parser, Debug)]
#[command(name = "radio")]
#[command(about = "A minimal internet radio player")]
struct Args {
    /// Path to the radios file
    #[arg(default_value = "radios.txt")]
    radios: String,

    /// Play in stereo (default is mono downmix)
    #[arg(long)]
    stereo: bool,

    /// File the EQ knob's curve is saved to and restored from
    #[arg(long, default_value = "eq.txt")]
    // Only the front panel, which exists on Linux, uses it.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    eq_file: std::path::PathBuf,
}

/// A station to fall back on when `radios.txt` is missing or unreadable.
///
/// An appliance that boots to silence because a file was renamed is an
/// appliance that needs a keyboard. One known-good station is a better
/// outcome, and the log says loudly what happened.
const FALLBACK_NAME: &str = "BBC World";
const FALLBACK_URL: &str = "http://stream.live.vc.bbcmedia.co.uk/bbc_world_service";

/// Render `text` as exactly [`DISPLAY_WIDTH`] bytes for the VFD.
///
/// Byte length, not character count, is what matters: the display takes raw
/// bytes and has no notion of UTF-8, so a station name with an umlaut in it
/// would otherwise send 13 bytes to a 12-cell display and shift everything
/// along. Anything non-ASCII becomes `?`, which is at least legible.
pub fn pad_display(text: &str) -> String {
    let mut out: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .take(DISPLAY_WIDTH)
        .collect();
    while out.len() < DISPLAY_WIDTH {
        out.push(' ');
    }
    out
}

pub fn volume_display_text(volume: f32) -> String {
    let vol = (volume * 10.0).round() as u32;
    pad_display(&format!("VOL {vol}"))
}

/// Render an EQ group and its offset, e.g. `LOWS +2` or `MIDS 0`.
pub fn eq_display_text(group: radio::eq::Group, offset: i32) -> String {
    let value = if offset == 0 {
        "0".to_string()
    } else {
        format!("{offset:+}")
    };
    pad_display(&format!("{} {value}", group.label()))
}

/// Render the current time as a centered `HH-MM`, padded within the display.
/// Centering reads naturally as a clock face; the hyphen is used rather than a
/// colon because the VFD's colon glyph is hard to read at a distance.
pub fn clock_display_text(now: &chrono::DateTime<chrono::Local>) -> String {
    let text = format!("{:^DISPLAY_WIDTH$}", now.format("%H-%M"));
    debug_assert_eq!(text.len(), DISPLAY_WIDTH);
    text
}

fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_level(true),
        )
        .with(env_filter)
        .init();
}

/// Route panics through `tracing` so they land in the journal.
///
/// By default a panic in a spawned task is swallowed: the task dies, tokio
/// logs nothing, and the symptom is a feature that silently stopped working.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "unknown location".to_string());
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>");
        error!("PANIC in thread '{name}' at {location}: {info}");
        default_hook(info);
    }));
}

/// Load the station list, falling back to a built-in station rather than
/// refusing to start.
fn load_radios(path: &str) -> RadioList {
    match RadioList::load(path) {
        Ok(list) => {
            info!("Loaded {} stations from {path}", list.len());
            list
        }
        Err(e) => {
            warn!("Could not load stations from {path}: {e} — using the built-in fallback");
            RadioList::single(radio::radio_list::Station {
                name: FALLBACK_NAME.to_string(),
                url: FALLBACK_URL.to_string(),
            })
        }
    }
}

/// Resolve once SIGINT or SIGTERM arrives.
///
/// SIGTERM is what systemd sends on `stop` and `restart`; without it the
/// display is never cleared and the Spotify session is never closed on an
/// ordinary service restart.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                warn!("Cannot listen for SIGTERM: {e}");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => info!("SIGINT received"),
            _ = term.recv() => info!("SIGTERM received"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    install_panic_hook();
    if let Err(e) = color_eyre::install() {
        warn!("color_eyre already installed: {e}");
    }

    // librespot pulls in both `ring` and `aws-lc-rs` rustls providers (ring via
    // hyper-rustls, aws-lc-rs via the oauth2->reqwest tree), so rustls cannot
    // auto-select one. Pin the `ring` provider to match librespot's own TLS
    // stack. Must run before any TLS connection is made.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let args = Args::parse();
    let radios = load_radios(&args.radios);
    let mono = !args.stereo;

    #[cfg(target_os = "linux")]
    {
        run_appliance(radios, mono, args.eq_file).await
    }

    // On a development machine there is no front panel; just play the first
    // station so the audio path can be exercised.
    #[cfg(not(target_os = "linux"))]
    {
        use radio::player::RodioPlayer;
        use radio::RadioApp;

        let player = RodioPlayer::new(mono)?;
        let app = RadioApp::new(radios, player);
        app.play_current();
        info!("Playing — press Ctrl-C to stop.");
        shutdown_signal().await;
        app.stop_playback();
        Ok(())
    }
}

#[cfg(target_os = "linux")]
async fn run_appliance(radios: RadioList, mono: bool, eq_file: std::path::PathBuf) -> Result<()> {
    use color_eyre::eyre::eyre;
    use radio::spotify::Spotify;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let shutdown = Arc::new(AtomicBool::new(false));

    // The panel thread owns the audio device (cpal handles are not `Send`) and
    // hands the shared sink back so Spotify can play through the same output.
    // A oneshot rather than a std channel so the wait below is a real await:
    // opening the audio device can take a minute of retries, and blocking the
    // runtime thread through all of it would also leave SIGTERM unhandled for
    // the whole window.
    let (sink_tx, sink_rx) = tokio::sync::oneshot::channel();
    let (spotify_tx, spotify_rx) = std::sync::mpsc::channel();

    let handle = tokio::runtime::Handle::current();
    let panel_shutdown = shutdown.clone();
    let panel = std::thread::Builder::new()
        .name("panel".to_string())
        .spawn(move || {
            panel::run(
                radios,
                mono,
                eq_file,
                handle,
                sink_tx,
                spotify_rx,
                panel_shutdown,
            );
        })?;

    // Wait for the audio device. The panel thread retries internally, so this
    // only fails if audio is truly unavailable — the one condition worth
    // exiting on, since systemd will restart us into a fresh attempt.
    let sink = tokio::select! {
        result = sink_rx => result
            .map_err(|_| eyre!("panel thread exited before an audio device was available"))?,
        () = shutdown_signal() => {
            info!("Asked to stop before audio was ready");
            shutdown.store(true, Ordering::Relaxed);
            let _ = panel.join();
            return Ok(());
        }
    };

    let spotify = Spotify::start(sink);
    if spotify_tx.send(spotify.handle()).is_err() {
        warn!("Panel thread went away before the Spotify handle was delivered");
    }

    shutdown_signal().await;
    info!("Shutting down");

    // Signal the panel first so it stops touching the Spotify handle, then
    // wind the receiver down.
    shutdown.store(true, Ordering::Relaxed);
    if panel.join().is_err() {
        warn!("Panel thread panicked during shutdown");
    }
    spotify.shutdown().await;

    info!("Stopped cleanly");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The display counts bytes, not characters, so every string we hand it
    /// must be exactly the panel width in *bytes*.
    #[test]
    fn display_helpers_always_fill_the_width_in_bytes() {
        for text in [
            "Flux FM",
            "",
            "A Very Long Station Name",
            "Öffentlich",             // non-ASCII, would be 13 bytes unpadded
            "Радио",                  // entirely non-ASCII
            "80s80s\u{00e9}\u{00e9}", // accents past the truncation point
        ] {
            let padded = pad_display(text);
            assert_eq!(padded.len(), DISPLAY_WIDTH, "byte length for {text:?}");
            assert!(padded.is_ascii(), "must be ASCII for the VFD: {padded:?}");
        }
    }

    #[test]
    fn clock_text_is_exactly_the_display_width() {
        let now = chrono::Local::now();
        assert_eq!(clock_display_text(&now).len(), DISPLAY_WIDTH);
    }

    #[test]
    fn eq_display_shows_group_and_signed_offset() {
        use radio::eq::{Group, OFFSET_LIMIT_DB};

        assert_eq!(eq_display_text(Group::Lows, 2).trim_end(), "LOWS +2");
        assert_eq!(eq_display_text(Group::Mids, 0).trim_end(), "MIDS 0");
        assert_eq!(eq_display_text(Group::Highs, -6).trim_end(), "HIGHS -6");
        for group in Group::ALL {
            for offset in [-OFFSET_LIMIT_DB, 0, OFFSET_LIMIT_DB] {
                assert_eq!(eq_display_text(group, offset).len(), DISPLAY_WIDTH);
            }
        }
    }

    #[test]
    fn volume_display_rounds_to_one_digit() {
        assert_eq!(volume_display_text(1.0).trim_end(), "VOL 10");
        assert_eq!(volume_display_text(0.0).trim_end(), "VOL 0");
        assert_eq!(volume_display_text(0.44).trim_end(), "VOL 4");
    }

    #[test]
    fn fallback_station_is_usable() {
        let list = load_radios("/nonexistent/definitely-not-here.txt");
        assert_eq!(list.len(), 1);
        let station = list.current().unwrap();
        assert_eq!(station.name, FALLBACK_NAME);
        assert!(station.url.starts_with("http"));
    }
}
