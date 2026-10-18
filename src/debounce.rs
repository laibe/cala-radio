//! Settle-time debouncing and edge detection for the panel push buttons.
//!
//! The Adafruit seesaw encoders expose their push button as a plain pull-up
//! GPIO — `button(0)` is `digital_read(pin 24)`, with no filtering in the
//! seesaw firmware. A poll fast enough to feel responsive is therefore also
//! fast enough to sample straight through the 1–10 ms of mechanical bounce on
//! every press *and* every release. Feeding those raw samples to a
//! `pressed && !was_pressed` edge detector turns one physical click into an
//! unpredictable number of edges: the symptom is a play/pause that toggles
//! twice per click, so playback only stays paused while the button is held.
//!
//! [`Button`] filters the level before any edge is reported: a new level has
//! to hold for the whole settle window before it is believed. Note that
//! polling *slower* is not a fix — it changes which bounces you happen to
//! sample, not whether you sample them.
//!
//! `now` is passed in rather than read from the clock inside, which keeps this
//! a pure state machine that can be tested without hardware.

use std::time::{Duration, Instant};

/// Default settle window. Comfortably longer than the bounce of the switches
/// in these encoders, and far shorter than a deliberate human click.
pub const DEFAULT_SETTLE: Duration = Duration::from_millis(25);

/// A debounced level transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// The button became pressed and stayed pressed.
    Press,
    /// The button became released and stayed released.
    Release,
}

/// Debounced view of a single push button.
#[derive(Debug, Clone)]
pub struct Button {
    settle: Duration,
    /// The level we currently believe.
    stable: bool,
    /// The level the raw samples have most recently been showing.
    candidate: bool,
    /// When `candidate` last changed.
    since: Instant,
}

impl Button {
    /// A button assumed released, with the default settle window.
    pub fn new(now: Instant) -> Self {
        Self::with_settle(DEFAULT_SETTLE, now)
    }

    pub fn with_settle(settle: Duration, now: Instant) -> Self {
        Self {
            settle,
            stable: false,
            candidate: false,
            since: now,
        }
    }

    /// Feed one raw sample. Returns an edge only once the new level has been
    /// held for the settle window.
    ///
    /// On a failed hardware read, simply don't call this: the last believed
    /// level holds, and no phantom edge is produced on the next successful
    /// read.
    pub fn update(&mut self, raw: bool, now: Instant) -> Option<Edge> {
        if raw != self.candidate {
            // Level moved — restart the settle timer. Bounce chatter lands
            // here repeatedly and never gets far enough to report an edge.
            self.candidate = raw;
            self.since = now;
            return None;
        }
        if self.candidate == self.stable {
            return None;
        }
        if now.duration_since(self.since) < self.settle {
            return None;
        }
        self.stable = self.candidate;
        Some(if self.stable {
            Edge::Press
        } else {
            Edge::Release
        })
    }

    /// The level currently believed.
    pub fn is_pressed(&self) -> bool {
        self.stable
    }

    /// Adopt `raw` as the believed level without reporting an edge.
    ///
    /// Used on a mode change so that a button held across the transition does
    /// not fire in the mode it was not pressed in.
    pub fn resync(&mut self, raw: bool, now: Instant) {
        self.stable = raw;
        self.candidate = raw;
        self.since = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETTLE: Duration = Duration::from_millis(25);

    fn button(t0: Instant) -> Button {
        Button::with_settle(SETTLE, t0)
    }

    /// The bug this module exists for: bounce chatter on press must produce
    /// exactly one edge, not one per rising sample.
    #[test]
    fn bouncing_press_yields_one_edge() {
        let t0 = Instant::now();
        let mut b = button(t0);
        let mut edges = Vec::new();

        // 40 ms of chatter sampled every 5 ms, then a steady hold.
        let chatter = [true, false, true, true, false, true, true, true];
        for (i, raw) in chatter.iter().enumerate() {
            let now = t0 + Duration::from_millis(5 * i as u64);
            edges.extend(b.update(*raw, now));
        }
        for i in 8..20 {
            let now = t0 + Duration::from_millis(5 * i as u64);
            edges.extend(b.update(true, now));
        }

        assert_eq!(edges, vec![Edge::Press]);
        assert!(b.is_pressed());
    }

    /// A full press-and-release cycle is one press edge and one release edge —
    /// so a toggle handler flips the state once, not twice.
    #[test]
    fn press_then_release_toggles_once() {
        let t0 = Instant::now();
        let mut b = button(t0);
        let mut edges = Vec::new();

        for i in 0..20 {
            edges.extend(b.update(true, t0 + Duration::from_millis(5 * i)));
        }
        // Release, with bounce on the way out too.
        let release = [false, true, false, false, true, false];
        for (i, raw) in release.iter().enumerate() {
            edges.extend(b.update(*raw, t0 + Duration::from_millis(100 + 5 * i as u64)));
        }
        for i in 0..20 {
            edges.extend(b.update(false, t0 + Duration::from_millis(130 + 5 * i)));
        }

        assert_eq!(edges, vec![Edge::Press, Edge::Release]);
        assert!(!b.is_pressed());
    }

    /// A spike shorter than the settle window is not a press.
    #[test]
    fn glitch_shorter_than_settle_is_ignored() {
        let t0 = Instant::now();
        let mut b = button(t0);
        assert_eq!(b.update(true, t0), None);
        assert_eq!(b.update(true, t0 + Duration::from_millis(10)), None);
        assert_eq!(b.update(false, t0 + Duration::from_millis(15)), None);
        for i in 0..10 {
            assert_eq!(
                b.update(false, t0 + Duration::from_millis(20 + 5 * i)),
                None
            );
        }
        assert!(!b.is_pressed());
    }

    /// Exactly at the settle boundary counts as settled.
    #[test]
    fn edge_fires_at_settle_boundary() {
        let t0 = Instant::now();
        let mut b = button(t0);
        assert_eq!(b.update(true, t0), None);
        assert_eq!(b.update(true, t0 + SETTLE - Duration::from_millis(1)), None);
        assert_eq!(b.update(true, t0 + SETTLE), Some(Edge::Press));
        // And it does not fire again while held.
        assert_eq!(
            b.update(true, t0 + SETTLE + Duration::from_millis(50)),
            None
        );
    }

    /// Dropped reads (which we model as simply not calling update) must not
    /// manufacture an edge when sampling resumes.
    #[test]
    fn gaps_in_sampling_do_not_manufacture_edges() {
        let t0 = Instant::now();
        let mut b = button(t0);
        for i in 0..20 {
            b.update(true, t0 + Duration::from_millis(5 * i));
        }
        assert!(b.is_pressed());
        // Long gap (simulating I2C errors), still held on the next sample.
        assert_eq!(b.update(true, t0 + Duration::from_secs(5)), None);
        assert!(b.is_pressed());
    }

    /// A button held across a mode change must not fire in the new mode.
    #[test]
    fn resync_adopts_level_without_edge() {
        let t0 = Instant::now();
        let mut b = button(t0);
        b.resync(true, t0);
        assert!(b.is_pressed());
        assert_eq!(b.update(true, t0 + Duration::from_millis(100)), None);
        // The following release is still reported normally.
        assert_eq!(b.update(false, t0 + Duration::from_millis(105)), None);
        assert_eq!(
            b.update(false, t0 + Duration::from_millis(140)),
            Some(Edge::Release)
        );
    }
}
