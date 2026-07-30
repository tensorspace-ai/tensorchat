//! Token-bucket rate limiting.
//!
//! Two shapes, because the two problems are different:
//!
//! * [`Bucket`] is a plain struct **owned by a connection task**. No locks, no
//!   map lookups, no allocation — the per-frame check is a subtraction. This is
//!   what guards the WebSocket message path, which is the only path hot enough
//!   for that to matter.
//! * [`IpLimiter`] is a sharded map keyed by client address, for
//!   pre-authentication HTTP endpoints where there is no connection to hang
//!   state off.
//!
//! Time comes from a monotonic [`Instant`], so a clock adjustment cannot hand
//! out free quota.

use std::net::IpAddr;
use std::time::{Duration, Instant};

use dashmap::DashMap;

/// A token bucket: `capacity` tokens, refilled at `per_second`.
///
/// Bursts up to `capacity` are allowed, which is what makes this pleasant for
/// interactive use — pasting three messages at once should not be throttled,
/// while a runaway loop still settles to the sustained rate.
#[derive(Debug, Clone)]
pub struct Bucket {
    capacity: f32,
    per_second: f32,
    tokens: f32,
    last: Instant,
}

impl Bucket {
    pub fn new(capacity: f32, per_second: f32) -> Bucket {
        Bucket {
            capacity,
            per_second,
            tokens: capacity,
            last: Instant::now(),
        }
    }

    /// Try to spend one token. Returns false when the caller should be
    /// throttled.
    pub fn allow(&mut self) -> bool {
        self.allow_n(1.0)
    }

    pub fn allow_n(&mut self, n: f32) -> bool {
        self.refill(Instant::now());
        if self.tokens >= n {
            self.tokens -= n;
            true
        } else {
            false
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f32();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.per_second).min(self.capacity);
            self.last = now;
        }
    }

    /// Tokens currently available, for tests and diagnostics.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn available(&self) -> f32 {
        self.tokens
    }
}

/// The limits applied to one WebSocket connection.
///
/// Separate buckets per frame class: a burst of typing indicators must not
/// consume the quota that lets you actually send a message.
pub struct ConnLimits {
    pub messages: Bucket,
    pub typing: Bucket,
    /// Everything else — subscribe, read receipts, reactions, pings.
    pub misc: Bucket,
}

impl Default for ConnLimits {
    fn default() -> Self {
        ConnLimits {
            // ~2 messages/sec sustained, bursting to 10. Comfortably above
            // human typing speed and far below what a script would want.
            messages: Bucket::new(10.0, 2.0),
            // Typing indicators are already client-throttled; this is a
            // backstop against a client that ignores that.
            typing: Bucket::new(5.0, 1.0),
            misc: Bucket::new(60.0, 20.0),
        }
    }
}

/// Per-IP limiter for unauthenticated endpoints (login, register).
///
/// Entries are pruned opportunistically: a background sweep would need its own
/// task and timer, whereas piggybacking on the check keeps the map bounded with
/// no extra machinery.
pub struct IpLimiter {
    buckets: DashMap<IpAddr, Bucket>,
    capacity: f32,
    per_second: f32,
    idle_evict: Duration,
}

impl IpLimiter {
    pub fn new(capacity: f32, per_second: f32) -> IpLimiter {
        IpLimiter {
            buckets: DashMap::new(),
            capacity,
            per_second,
            idle_evict: Duration::from_secs(600),
        }
    }

    pub fn allow(&self, ip: IpAddr) -> bool {
        // Sweep occasionally rather than on every call: at a few hundred
        // entries the scan is trivial, and this keeps a long-running server
        // from accumulating one entry per IP that ever touched it.
        if self.buckets.len() > 4096 {
            let cutoff = Instant::now() - self.idle_evict;
            self.buckets.retain(|_, b| b.last > cutoff);
        }
        let mut entry = self
            .buckets
            .entry(ip)
            .or_insert_with(|| Bucket::new(self.capacity, self.per_second));
        entry.allow()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn tracked(&self) -> usize {
        self.buckets.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn allows_a_burst_then_throttles() {
        let mut b = Bucket::new(3.0, 1.0);
        assert!(b.allow());
        assert!(b.allow());
        assert!(b.allow());
        assert!(!b.allow(), "burst capacity exhausted");
    }

    #[test]
    fn refills_over_time() {
        let mut b = Bucket::new(2.0, 100.0);
        assert!(b.allow());
        assert!(b.allow());
        assert!(!b.allow());
        std::thread::sleep(Duration::from_millis(50));
        assert!(b.allow(), "should have refilled after 50ms at 100/s");
    }

    #[test]
    fn refill_never_exceeds_capacity() {
        let mut b = Bucket::new(5.0, 1000.0);
        std::thread::sleep(Duration::from_millis(20));
        b.refill(Instant::now());
        assert!(b.available() <= 5.0, "got {}", b.available());
    }

    #[test]
    fn separate_frame_classes_do_not_share_quota() {
        let mut l = ConnLimits::default();
        // Burn the entire typing allowance.
        while l.typing.allow() {}
        assert!(
            l.messages.allow(),
            "typing spam must not block real messages"
        );
    }

    #[test]
    fn ip_limiter_is_per_address() {
        let l = IpLimiter::new(2.0, 0.001);
        let a = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let b = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));
        assert!(l.allow(a));
        assert!(l.allow(a));
        assert!(!l.allow(a), "first address is throttled");
        assert!(l.allow(b), "a different address has its own bucket");
        assert_eq!(l.tracked(), 2);
    }

    #[test]
    fn a_zero_rate_bucket_never_refills() {
        let mut b = Bucket::new(1.0, 0.0);
        assert!(b.allow());
        std::thread::sleep(Duration::from_millis(10));
        assert!(!b.allow());
    }
}
