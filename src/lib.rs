#![warn(clippy::unwrap_used, clippy::expect_used)]

#[cfg(target_os = "linux")]
pub mod amp_eq;
pub mod debounce;
#[cfg(target_os = "linux")]
pub mod enc8421;
#[cfg(target_os = "linux")]
pub mod encoders;
pub mod eq;
pub mod mode;
pub mod player;
pub mod radio_list;
pub mod spotify;
#[cfg(test)]
pub mod testing;
#[cfg(unix)]
pub mod watchdog;

use player::Player;
use radio_list::RadioList;

pub struct RadioApp<P: Player> {
    radios: RadioList,
    player: P,
    volume: f32,
}

impl<P: Player> RadioApp<P> {
    pub fn new(radios: RadioList, player: P) -> Self {
        Self {
            radios,
            player,
            volume: 1.0,
        }
    }

    /// Stop the current station's playback (halts streaming and decoding).
    pub fn stop_playback(&self) {
        self.player.stop();
    }

    pub fn next_station(&mut self) {
        self.radios.next();
        self.play_current();
    }

    pub fn prev_station(&mut self) {
        self.radios.prev();
        self.play_current();
    }

    /// Start playing whatever station is currently selected.
    ///
    /// Note this is `current`, not `first`: after a station change, entering
    /// Radio mode should resume the station you left on.
    pub fn play_current(&self) {
        if let Some(station) = self.radios.current() {
            self.player.stop();
            self.player.play(&station.url);
            self.player.set_volume(self.volume);
        }
    }

    /// Move the selection without touching playback.
    ///
    /// Selection and playback are separate so the panel can follow the knob
    /// immediately while waiting for it to settle before connecting — turning
    /// past five stations should not open five connections.
    pub fn select_relative(&mut self, steps: i32) {
        self.radios.advance(steps);
    }

    /// Adjust the current volume by a delta, clamp it, and apply it.
    ///
    /// This deliberately does not resume from pause: the sink is shared, and
    /// in Streamer mode the sink being adjusted is not the one the user is
    /// listening to. Callers that want that behaviour ask for it with
    /// [`Self::resume`].
    pub fn change_volume(&mut self, delta: f32) -> f32 {
        self.set_volume(self.volume + delta);
        self.volume
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 2.0);
        self.player.set_volume(self.volume);
    }

    /// Ensure the output is not paused.
    ///
    /// Also used when entering a mode that plays audio: the sink is shared
    /// with Spotify, so a sink left paused by the radio would swallow
    /// Spotify's audio as well.
    pub fn resume(&self) {
        self.player.resume();
    }

    pub fn current_station(&self) -> Option<&radio_list::Station> {
        self.radios.current()
    }

    /// Toggle pause/play, returning the new paused state.
    pub fn toggle_playback(&mut self) -> bool {
        self.player.toggle_pause()
    }

    /// Whether playback is currently paused.
    pub fn is_paused(&self) -> bool {
        self.player.is_paused()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use player::Player;
    use radio_list::Station;
    use std::sync::{Arc, Mutex};

    #[derive(Default, Clone)]
    struct MockPlayer {
        calls: Arc<Mutex<Vec<String>>>,
        paused: Arc<Mutex<bool>>,
    }

    impl Player for MockPlayer {
        fn play(&self, url: &str) {
            self.calls.lock().unwrap().push(format!("play:{url}"));
        }
        fn stop(&self) {
            self.calls.lock().unwrap().push("stop".to_string());
        }
        fn set_volume(&self, volume: f32) {
            self.calls.lock().unwrap().push(format!("volume:{volume}"));
        }
        fn toggle_pause(&self) -> bool {
            self.calls.lock().unwrap().push("toggle_pause".to_string());
            let mut p = self.paused.lock().unwrap();
            *p = !*p;
            *p
        }
        fn is_paused(&self) -> bool {
            *self.paused.lock().unwrap()
        }
        fn resume(&self) {
            self.calls.lock().unwrap().push("resume".to_string());
            *self.paused.lock().unwrap() = false;
        }
    }

    fn make_test_radios() -> RadioList {
        RadioList::from_stations(vec![
            Station {
                name: "First".into(),
                url: "http://first.com".into(),
            },
            Station {
                name: "Second".into(),
                url: "http://second.com".into(),
            },
        ])
        .unwrap()
    }

    #[test]
    fn test_play_current() {
        let radios = make_test_radios();
        let player = MockPlayer::default();
        let app = RadioApp::new(radios, player.clone());
        app.play_current();
        let calls = player.calls.lock().unwrap();
        assert_eq!(calls[0], "stop");
        assert_eq!(calls[1], "play:http://first.com");
        assert_eq!(calls[2], "volume:1");
    }

    #[test]
    fn test_next_station() {
        let radios = make_test_radios();
        let player = MockPlayer::default();
        let mut app = RadioApp::new(radios, player.clone());
        app.next_station();
        let calls = player.calls.lock().unwrap();
        assert_eq!(calls[0], "stop");
        assert_eq!(calls[1], "play:http://second.com");
        assert_eq!(calls[2], "volume:1");
    }

    #[test]
    fn test_prev_station() {
        let radios = make_test_radios();
        let player = MockPlayer::default();
        let mut app = RadioApp::new(radios, player.clone());
        app.prev_station();
        let calls = player.calls.lock().unwrap();
        assert_eq!(calls[0], "stop");
        assert_eq!(calls[1], "play:http://second.com");
        assert_eq!(calls[2], "volume:1");
    }

    #[test]
    fn test_set_volume() {
        let radios = make_test_radios();
        let player = MockPlayer::default();
        let mut app = RadioApp::new(radios, player.clone());
        app.set_volume(0.5);
        let calls = player.calls.lock().unwrap();
        assert_eq!(calls[0], "volume:0.5");
    }

    #[test]
    fn test_set_volume_clamped() {
        let radios = make_test_radios();
        let player = MockPlayer::default();
        let mut app = RadioApp::new(radios, player.clone());
        app.set_volume(5.0);
        let calls = player.calls.lock().unwrap();
        assert_eq!(calls[0], "volume:2");
    }

    #[test]
    fn test_current_station() {
        let radios = make_test_radios();
        let player = MockPlayer::default();
        let app = RadioApp::new(radios, player);
        let st = app.current_station().unwrap();
        assert_eq!(st.name, "First");
        assert_eq!(st.url, "http://first.com");
    }

    #[test]
    fn test_change_volume() {
        let radios = make_test_radios();
        let player = MockPlayer::default();
        let mut app = RadioApp::new(radios, player.clone());
        app.change_volume(-0.3);
        let calls = player.calls.lock().unwrap();
        assert_eq!(calls[0], "volume:0.7");
    }

    #[test]
    fn test_toggle_playback() {
        let radios = make_test_radios();
        let player = MockPlayer::default();
        let mut app = RadioApp::new(radios, player.clone());
        let paused = app.toggle_playback();
        assert!(paused);
        let calls = player.calls.lock().unwrap();
        assert_eq!(calls[0], "toggle_pause");
    }

    /// Changing the volume must not un-pause as a side effect: the sink
    /// is shared with Spotify, so that behaviour reached across modes.
    #[test]
    fn test_change_volume_does_not_resume() {
        let radios = make_test_radios();
        let player = MockPlayer::default();
        let mut app = RadioApp::new(radios, player.clone());
        app.toggle_playback();
        player.calls.lock().unwrap().clear();

        app.change_volume(-0.3);
        assert!(app.is_paused(), "still paused");
        let calls = player.calls.lock().unwrap();
        assert_eq!(calls.as_slice(), ["volume:0.7"]);
    }

    #[test]
    fn test_resume_is_explicit_and_idempotent() {
        let radios = make_test_radios();
        let player = MockPlayer::default();
        let mut app = RadioApp::new(radios, player.clone());
        app.toggle_playback();
        assert!(app.is_paused());

        app.resume();
        assert!(!app.is_paused());
        app.resume();
        assert!(!app.is_paused());
    }
}
