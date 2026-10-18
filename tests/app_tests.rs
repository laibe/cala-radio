use clap::Parser;
use radio::player::Player;
use radio::radio_list::{RadioList, Station};
use radio::RadioApp;
use std::sync::{Arc, Mutex};

#[derive(Default, Clone)]
struct MockPlayer {
    calls: Arc<Mutex<Vec<String>>>,
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
        true
    }
    fn is_paused(&self) -> bool {
        false
    }
    fn resume(&self) {
        self.calls.lock().unwrap().push("resume".to_string());
    }
}

#[derive(Parser, Debug)]
#[command(name = "radio")]
#[command(about = "A minimal internet radio player")]
struct Args {
    #[arg(default_value = "radios.txt")]
    radios: String,
}

#[test]
fn test_cli_parsing_default() {
    let args = Args::try_parse_from(["radio"]).unwrap();
    assert_eq!(args.radios, "radios.txt");
}

#[test]
fn test_cli_parsing_custom_path() {
    let args = Args::try_parse_from(["radio", "my-radios.txt"]).unwrap();
    assert_eq!(args.radios, "my-radios.txt");
}

#[test]
fn test_integration_app_with_mock_player() {
    let radios = RadioList::from_stations(vec![
        Station {
            name: "One".into(),
            url: "http://station-one.com".into(),
        },
        Station {
            name: "Two".into(),
            url: "http://station-two.com".into(),
        },
    ])
    .unwrap();
    let player = MockPlayer::default();
    let app = RadioApp::new(radios, player.clone());
    app.play_current();
    app.play_current();

    let calls = player.calls.lock().unwrap();
    assert_eq!(calls.len(), 6);
    assert_eq!(calls[0], "stop");
    assert_eq!(calls[1], "play:http://station-one.com");
    assert_eq!(calls[2], "volume:1");
}

#[test]
fn test_integration_station_navigation() {
    let radios = RadioList::from_stations(vec![
        Station {
            name: "First".into(),
            url: "http://first.com".into(),
        },
        Station {
            name: "Second".into(),
            url: "http://second.com".into(),
        },
        Station {
            name: "Third".into(),
            url: "http://third.com".into(),
        },
    ])
    .unwrap();
    let player = MockPlayer::default();
    let mut app = RadioApp::new(radios, player.clone());

    app.next_station();
    app.next_station();
    app.prev_station();

    let calls = player.calls.lock().unwrap();
    // next -> second
    assert_eq!(calls[0], "stop");
    assert_eq!(calls[1], "play:http://second.com");
    assert_eq!(calls[2], "volume:1");
    // next -> third
    assert_eq!(calls[3], "stop");
    assert_eq!(calls[4], "play:http://third.com");
    assert_eq!(calls[5], "volume:1");
    // prev -> second
    assert_eq!(calls[6], "stop");
    assert_eq!(calls[7], "play:http://second.com");
    assert_eq!(calls[8], "volume:1");
}

#[test]
fn test_integration_station_names() {
    let radios = RadioList::from_stations(vec![
        Station {
            name: "Classic Rock".into(),
            url: "http://rock.com".into(),
        },
        Station {
            name: "Jazz FM".into(),
            url: "http://jazz.com".into(),
        },
    ])
    .unwrap();
    let player = MockPlayer::default();
    let app = RadioApp::new(radios, player);
    let st = app.current_station().unwrap();
    assert_eq!(st.name, "Classic Rock");
    assert_eq!(st.url, "http://rock.com");
}
