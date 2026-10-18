use color_eyre::eyre::{eyre, Result};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Station {
    pub name: String,
    pub url: String,
}

impl Station {
    /// Parse a line from the radios file.
    ///
    /// Format: `Name,URL`  or just `URL`
    /// If no comma is present, the whole line is treated as the URL and the
    /// host portion is used as the display name.
    pub fn parse_line(line: &str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        if let Some((name, url)) = line.split_once(',') {
            let name = name.trim().to_string();
            let url = url.trim().to_string();
            if name.is_empty() || url.is_empty() {
                return None;
            }
            Some(Self { name, url })
        } else {
            let url = line.to_string();
            // Use host as fallback name.
            let name = url
                .trim_start_matches("http://")
                .trim_start_matches("https://")
                .split('/')
                .next()
                .unwrap_or(&url)
                .to_string();
            Some(Self { name, url })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadioList {
    stations: Vec<Station>,
    current: usize,
}

impl RadioList {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Self::load_from_str(&content)
    }

    /// Parse a station list from text. Used for the built-in fallback station,
    /// so a missing or unreadable `radios.txt` does not mean silence.
    pub fn load_from_str(content: &str) -> Result<Self> {
        let stations: Vec<Station> = content.lines().filter_map(Station::parse_line).collect();
        Self::from_stations(stations)
    }

    /// A list of exactly one station. Infallible by construction, which is
    /// what lets the built-in fallback path contain no panic at all.
    pub fn single(station: Station) -> Self {
        Self {
            stations: vec![station],
            current: 0,
        }
    }

    pub fn from_stations(stations: Vec<Station>) -> Result<Self> {
        if stations.is_empty() {
            return Err(eyre!("No radio stations provided"));
        }
        Ok(Self {
            stations,
            current: 0,
        })
    }

    pub fn current(&self) -> Option<&Station> {
        self.stations.get(self.current)
    }

    /// Move `steps` places through the list, wrapping, where a negative value
    /// moves backwards.
    ///
    /// Used for the select knob: it reports an absolute position, so a fast
    /// turn (or a poll that lands after several detents) is a jump of more
    /// than one. Stepping only once per poll made the knob feel like it was
    /// dropping detents.
    pub fn advance(&mut self, steps: i32) {
        let len = self.stations.len();
        if len == 0 || steps == 0 {
            return;
        }
        let len_i = len as i64;
        let current = self.current as i64;
        let target = (current + steps as i64).rem_euclid(len_i);
        self.current = target as usize;
    }

    // The constructor rejects an empty list, so these guards are currently
    // unreachable. They stay because the alternative if that invariant ever
    // slips is a modulo-by-zero panic in the panel loop, which on this device
    // means silence until someone power-cycles it.
    pub fn next(&mut self) {
        if !self.stations.is_empty() {
            self.current = (self.current + 1) % self.stations.len();
        }
    }

    pub fn prev(&mut self) {
        if !self.stations.is_empty() {
            self.current = if self.current == 0 {
                self.stations.len() - 1
            } else {
                self.current - 1
            };
        }
    }

    pub fn len(&self) -> usize {
        self.stations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stations.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_parse_line_name_url() {
        let s = Station::parse_line("My Station,http://example.com/1").unwrap();
        assert_eq!(s.name, "My Station");
        assert_eq!(s.url, "http://example.com/1");
    }

    #[test]
    fn test_parse_line_url_only() {
        let s = Station::parse_line("http://example.com/1").unwrap();
        assert_eq!(s.name, "example.com");
        assert_eq!(s.url, "http://example.com/1");
    }

    #[test]
    fn test_parse_line_empty() {
        assert!(Station::parse_line("").is_none());
        assert!(Station::parse_line("   ").is_none());
    }

    #[test]
    fn test_parse_line_missing_url() {
        assert!(Station::parse_line("NameOnly,").is_none());
        assert!(Station::parse_line(",http://example.com").is_none());
    }

    #[test]
    fn test_load_radios_with_names() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "Station A,http://example.com/1").unwrap();
        writeln!(file, "http://example.com/2").unwrap();
        let list = RadioList::load(file.path()).unwrap();
        assert_eq!(list.len(), 2);
        let first = list.current().unwrap();
        assert_eq!(first.name, "Station A");
        assert_eq!(first.url, "http://example.com/1");
    }

    #[test]
    fn test_next_wraps() {
        let list = RadioList::from_stations(vec![
            Station {
                name: "A".into(),
                url: "http://a.com".into(),
            },
            Station {
                name: "B".into(),
                url: "http://b.com".into(),
            },
        ])
        .unwrap();
        let mut list = list;
        list.next();
        assert_eq!(list.current().unwrap().name, "B");
        list.next();
        assert_eq!(list.current().unwrap().name, "A");
    }

    #[test]
    fn test_prev_wraps() {
        let list = RadioList::from_stations(vec![
            Station {
                name: "A".into(),
                url: "http://a.com".into(),
            },
            Station {
                name: "B".into(),
                url: "http://b.com".into(),
            },
        ])
        .unwrap();
        let mut list = list;
        list.prev();
        assert_eq!(list.current().unwrap().name, "B");
        list.prev();
        assert_eq!(list.current().unwrap().name, "A");
    }

    #[test]
    fn test_empty_file_fails() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file).unwrap();
        writeln!(file, "   ").unwrap();
        assert!(RadioList::load(file.path()).is_err());
    }

    #[test]
    fn test_from_stations_empty_fails() {
        assert!(RadioList::from_stations(vec![]).is_err());
    }

    #[test]
    fn test_advance_wraps_in_both_directions() {
        let mut list =
            RadioList::load_from_str("A,http://a\nB,http://b\nC,http://c\nD,http://d\n").unwrap();
        list.advance(2);
        assert_eq!(list.current().unwrap().name, "C");
        list.advance(3); // wraps past the end
        assert_eq!(list.current().unwrap().name, "B");
        list.advance(-3); // wraps past the start
        assert_eq!(list.current().unwrap().name, "C");
        list.advance(0);
        assert_eq!(list.current().unwrap().name, "C");
        list.advance(-9);
        assert_eq!(list.current().unwrap().name, "B");
    }

    #[test]
    fn test_advance_matches_repeated_next() {
        let mut a = RadioList::load_from_str("A,http://a\nB,http://b\nC,http://c\n").unwrap();
        let mut b = a.clone();
        a.advance(5);
        for _ in 0..5 {
            b.next();
        }
        assert_eq!(a.current(), b.current());
    }

    #[test]
    fn test_load_from_str() {
        let list = RadioList::load_from_str("A,http://a.example\n\nB,http://b.example\n").unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list.current().unwrap().name, "A");
        assert!(RadioList::load_from_str("   \n").is_err());
    }
}
