//! systemd readiness and watchdog notifications, without a dependency.
//!
//! `Restart=always` handles a process that *dies*. It cannot see a process
//! that is still running but wedged — which, for this program, is the failure
//! mode that actually strands the radio (a blocked I2C transaction, a control
//! loop that stopped ticking). `WatchdogSec` closes that gap: if the pings
//! stop arriving, systemd kills and restarts us.
//!
//! The protocol is one line of text sent to the datagram socket named by
//! `$NOTIFY_SOCKET`, so it needs no crate. Everything here degrades to a no-op
//! when the variable is absent, which is what happens when the binary is run
//! by hand.

use std::os::unix::net::UnixDatagram;
use std::time::Duration;

use tracing::{debug, info, warn};

/// Sends `READY=1` and `WATCHDOG=1` to systemd, when systemd is listening.
pub struct Watchdog {
    socket: Option<UnixDatagram>,
    address: String,
    /// How often to ping, if the service asked for a watchdog.
    interval: Option<Duration>,
}

impl Watchdog {
    /// Build from the environment systemd provides.
    pub fn from_env() -> Self {
        let notify_socket = std::env::var("NOTIFY_SOCKET").ok();
        let watchdog_usec = std::env::var("WATCHDOG_USEC")
            .ok()
            .and_then(|v| v.parse::<u64>().ok());
        // WATCHDOG_PID guards against inheriting the variable into a child.
        let for_us = std::env::var("WATCHDOG_PID")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .is_none_or(|pid| pid == std::process::id());
        Self::new(notify_socket, watchdog_usec.filter(|_| for_us))
    }

    /// Split out from [`Self::from_env`] so the behaviour is testable without
    /// mutating process-wide environment state.
    pub fn new(notify_socket: Option<String>, watchdog_usec: Option<u64>) -> Self {
        let Some(address) = notify_socket else {
            debug!("No NOTIFY_SOCKET; systemd notifications disabled");
            return Self::disabled();
        };

        // systemd may hand us an abstract socket (leading '@'), which std
        // cannot address portably. Not fatal — we simply stay quiet.
        if address.starts_with('@') {
            warn!("NOTIFY_SOCKET is an abstract socket; systemd notifications disabled");
            return Self::disabled();
        }

        let socket = match UnixDatagram::unbound() {
            Ok(s) => s,
            Err(e) => {
                warn!("Cannot open the systemd notify socket: {e}");
                return Self::disabled();
            }
        };

        // Ping at a third of the deadline, so a single missed tick is not
        // instantly fatal.
        let interval = watchdog_usec
            .filter(|usec| *usec > 0)
            .map(|usec| Duration::from_micros(usec / 3));
        if let Some(interval) = interval {
            info!("systemd watchdog enabled; pinging every {interval:.0?}");
        }

        Self {
            socket: Some(socket),
            address,
            interval,
        }
    }

    fn disabled() -> Self {
        Self {
            socket: None,
            address: String::new(),
            interval: None,
        }
    }

    fn send(&self, message: &str) {
        let Some(socket) = self.socket.as_ref() else {
            return;
        };
        if let Err(e) = socket.send_to(message.as_bytes(), &self.address) {
            debug!("systemd notify failed: {e}");
        }
    }

    /// Tell systemd startup finished. Required for `Type=notify`.
    pub fn ready(&self) {
        self.send("READY=1\n");
    }

    /// Tell systemd we are still making progress.
    pub fn ping(&self) {
        if self.interval.is_some() {
            self.send("WATCHDOG=1\n");
        }
    }

    /// Tell systemd we are going away on purpose, so a slow shutdown is not
    /// mistaken for a hang.
    pub fn stopping(&self) {
        self.send("STOPPING=1\n");
    }

    /// How often [`Self::ping`] should be called, if at all.
    pub fn interval(&self) -> Option<Duration> {
        self.interval
    }

    pub fn is_enabled(&self) -> bool {
        self.socket.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixDatagram;

    #[test]
    fn disabled_without_a_notify_socket() {
        let wd = Watchdog::new(None, Some(30_000_000));
        assert!(!wd.is_enabled());
        assert_eq!(wd.interval(), None);
        // Must be safe to call anyway — this is the "run it by hand" path.
        wd.ready();
        wd.ping();
    }

    #[test]
    fn abstract_sockets_are_declined_not_fatal() {
        let wd = Watchdog::new(Some("@/org/freedesktop/systemd1/notify".into()), Some(1));
        assert!(!wd.is_enabled());
        wd.ready();
    }

    #[test]
    fn pings_only_when_a_watchdog_was_requested() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify");
        let listener = UnixDatagram::bind(&path).unwrap();
        listener
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();

        let wd = Watchdog::new(Some(path.to_string_lossy().into_owned()), None);
        assert!(wd.is_enabled());
        assert_eq!(wd.interval(), None);

        wd.ready();
        let mut buf = [0u8; 64];
        let n = listener.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"READY=1\n");

        // No WATCHDOG_USEC means no watchdog traffic at all.
        wd.ping();
        assert!(
            listener.recv(&mut buf).is_err(),
            "ping must be silent when no watchdog was requested"
        );
    }

    #[test]
    fn sends_ready_and_watchdog_pings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify");
        let listener = UnixDatagram::bind(&path).unwrap();
        listener
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();

        // 30 s deadline -> ping every 10 s.
        let wd = Watchdog::new(Some(path.to_string_lossy().into_owned()), Some(30_000_000));
        assert!(wd.is_enabled());
        assert_eq!(wd.interval(), Some(Duration::from_secs(10)));

        wd.ready();
        let mut buf = [0u8; 64];
        let n = listener.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"READY=1\n");

        wd.ping();
        let n = listener.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"WATCHDOG=1\n");

        wd.stopping();
        let n = listener.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"STOPPING=1\n");
    }

    /// Without WATCHDOG_USEC (or with one meant for another PID, which
    /// `from_env` filters out) there is no ping interval.
    #[test]
    fn no_watchdog_usec_means_no_interval() {
        let wd = Watchdog::new(Some("/nonexistent/notify".into()), None);
        assert_eq!(wd.interval(), None);
    }
}
