//! Runtime-neutral native WebSocket heartbeat state.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;

use crate::Config;

static NEXT_NONCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Deadline {
    Pong(u64),
    Idle(u64),
    Ping(u64),
}

impl Deadline {
    pub(crate) fn at(self) -> u64 {
        match self {
            Self::Pong(at) | Self::Idle(at) | Self::Ping(at) => at,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Heartbeat {
    auto_ping: bool,
    ping_interval_ms: u64,
    idle_timeout_ms: u64,
    pong_timeout_ms: u64,
    last_inbound_ms: u64,
    outstanding: Option<OutstandingPing>,
    stopped: bool,
}

#[derive(Debug)]
struct OutstandingPing {
    payload: Bytes,
    deadline_ms: Option<u64>,
    flushed: bool,
}

impl Heartbeat {
    pub(crate) fn new(config: &Config, now_ms: u64) -> Self {
        Self {
            auto_ping: config.auto_ping && config.ping_interval != 0,
            ping_interval_ms: seconds_ms(config.ping_interval),
            idle_timeout_ms: seconds_ms(config.idle_timeout),
            pong_timeout_ms: seconds_ms(config.pong_timeout),
            last_inbound_ms: now_ms,
            outstanding: None,
            stopped: false,
        }
    }

    pub(crate) fn next_deadline(&self) -> Option<Deadline> {
        if self.stopped {
            return None;
        }

        // A Pong timeout deliberately wins ties with the hard idle timeout.
        if let Some(deadline) = self.outstanding.as_ref().and_then(|ping| ping.deadline_ms) {
            return Some(Deadline::Pong(deadline));
        }

        let idle = (self.idle_timeout_ms != 0)
            .then(|| self.last_inbound_ms.saturating_add(self.idle_timeout_ms));
        let ping = (self.auto_ping && self.outstanding.is_none())
            .then(|| self.last_inbound_ms.saturating_add(self.ping_interval_ms));

        match (idle, ping) {
            (Some(idle), Some(ping)) if idle <= ping => Some(Deadline::Idle(idle)),
            (Some(_), Some(ping)) => Some(Deadline::Ping(ping)),
            (Some(idle), None) => Some(Deadline::Idle(idle)),
            (None, Some(ping)) => Some(Deadline::Ping(ping)),
            (None, None) => None,
        }
    }

    pub(crate) fn ping_due(&mut self, now_ms: u64) -> Option<Bytes> {
        if !matches!(self.next_deadline(), Some(Deadline::Ping(at)) if at <= now_ms) {
            return None;
        }

        let nonce = NEXT_NONCE.fetch_add(1, Ordering::Relaxed);
        let payload = Bytes::copy_from_slice(&nonce.to_be_bytes());
        self.outstanding = Some(OutstandingPing {
            payload: payload.clone(),
            deadline_ms: None,
            flushed: false,
        });
        Some(payload)
    }

    pub(crate) fn ping_flushed(&mut self, now_ms: u64) {
        if let Some(ping) = &mut self.outstanding
            && !ping.flushed
        {
            ping.flushed = true;
            ping.deadline_ms =
                (self.pong_timeout_ms != 0).then(|| now_ms.saturating_add(self.pong_timeout_ms));
        }
    }

    /// Record a decoded inbound frame.
    ///
    /// Every valid inbound frame resets inactivity. Only the exact Pong for the
    /// currently outstanding, successfully flushed Ping clears its deadline.
    pub(crate) fn on_inbound(&mut self, now_ms: u64, pong: Option<&Bytes>) -> bool {
        if self.stopped {
            return false;
        }

        self.last_inbound_ms = now_ms;
        let matched = self.outstanding.as_ref().is_some_and(|ping| {
            ping.flushed
                && ping.deadline_ms.is_none_or(|deadline| now_ms < deadline)
                && pong.is_some_and(|payload| payload == &ping.payload)
        });
        if matched {
            self.outstanding = None;
        }
        matched
    }

    pub(crate) fn stop(&mut self) {
        self.stopped = true;
        self.outstanding = None;
    }

    #[cfg(test)]
    pub(crate) fn has_outstanding_ping(&self) -> bool {
        self.outstanding.is_some()
    }
}

fn seconds_ms(seconds: u32) -> u64 {
    Duration::from_secs(seconds.into()).as_millis() as u64
}

pub(crate) fn bounded_close_reason(reason: &str) -> String {
    const MAX_REASON_BYTES: usize = 123;
    if reason.len() <= MAX_REASON_BYTES {
        return reason.to_owned();
    }

    let mut end = MAX_REASON_BYTES;
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::builder()
            .idle_timeout(0)
            .ping_interval(10)
            .pong_timeout(5)
            .build()
    }

    #[test]
    fn inactivity_and_matching_pong_cycle() {
        let mut heartbeat = Heartbeat::new(&config(), 0);
        assert_eq!(heartbeat.next_deadline(), Some(Deadline::Ping(10_000)));
        assert!(heartbeat.ping_due(9_999).is_none());

        let payload = heartbeat.ping_due(10_000).unwrap();
        assert!(heartbeat.has_outstanding_ping());
        assert_eq!(heartbeat.next_deadline(), None);

        heartbeat.ping_flushed(10_100);
        assert_eq!(heartbeat.next_deadline(), Some(Deadline::Pong(15_100)));
        assert!(!heartbeat.on_inbound(11_000, Some(&Bytes::from_static(b"wrong"))));
        assert_eq!(heartbeat.next_deadline(), Some(Deadline::Pong(15_100)));
        assert!(heartbeat.on_inbound(12_000, Some(&payload)));
        assert_eq!(heartbeat.next_deadline(), Some(Deadline::Ping(22_000)));
    }

    #[test]
    fn traffic_resets_inactivity_but_not_pong_deadline() {
        let mut heartbeat = Heartbeat::new(&config(), 0);
        let _ = heartbeat.ping_due(10_000).unwrap();
        heartbeat.ping_flushed(10_000);
        heartbeat.on_inbound(12_000, None);
        assert_eq!(heartbeat.next_deadline(), Some(Deadline::Pong(15_000)));
    }

    #[test]
    fn zero_pong_timeout_keeps_one_ping_outstanding_without_deadline() {
        let config = Config::builder()
            .idle_timeout(0)
            .ping_interval(1)
            .pong_timeout(0)
            .build();
        let mut heartbeat = Heartbeat::new(&config, 0);
        let _ = heartbeat.ping_due(1_000).unwrap();
        heartbeat.ping_flushed(1_000);
        assert_eq!(heartbeat.next_deadline(), None);
        assert!(heartbeat.has_outstanding_ping());
    }

    #[test]
    fn pong_timeout_wins_idle_timeout_tie() {
        let config = Config::builder()
            .ping_interval(1)
            .pong_timeout(2)
            .idle_timeout(3)
            .build();
        let mut heartbeat = Heartbeat::new(&config, 0);
        let _ = heartbeat.ping_due(1_000).unwrap();
        heartbeat.ping_flushed(1_000);
        assert_eq!(heartbeat.next_deadline(), Some(Deadline::Pong(3_000)));
    }

    #[test]
    fn close_reason_is_bounded_at_a_utf8_boundary() {
        let reason = "x".repeat(122) + "€";
        let bounded = bounded_close_reason(&reason);
        assert_eq!(bounded.len(), 122);
        assert!(bounded.is_char_boundary(bounded.len()));
    }

    #[test]
    fn unsolicited_stale_and_late_pongs_do_not_match() {
        let mut heartbeat = Heartbeat::new(&config(), 0);
        assert!(!heartbeat.on_inbound(1_000, Some(&Bytes::from_static(b"unsolicited"))));

        let first = heartbeat.ping_due(11_000).unwrap();
        heartbeat.ping_flushed(11_000);
        assert!(heartbeat.on_inbound(12_000, Some(&first)));

        let second = heartbeat.ping_due(22_000).unwrap();
        heartbeat.ping_flushed(22_000);
        assert_ne!(first, second);
        assert!(!heartbeat.on_inbound(23_000, Some(&first)));
        assert!(!heartbeat.on_inbound(27_000, Some(&second)));
        assert!(heartbeat.has_outstanding_ping());
    }

    #[test]
    fn disabled_proactive_ping_and_zero_hard_idle_have_no_deadline() {
        let config = Config::builder()
            .auto_ping(false)
            .ping_interval(1)
            .idle_timeout(0)
            .build();
        assert_eq!(Heartbeat::new(&config, 0).next_deadline(), None);

        let zero_interval = Config::builder()
            .auto_ping(true)
            .ping_interval(0)
            .idle_timeout(0)
            .build();
        assert_eq!(Heartbeat::new(&zero_interval, 0).next_deadline(), None);
    }
}
