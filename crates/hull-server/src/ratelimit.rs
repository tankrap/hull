//! A tiny in-memory fixed-window rate limiter. Used to throttle the unauthenticated
//! `sovereign/wrapped` endpoint per client IP; the per-account offline-attack resistance still rests
//! on the client Argon2id KDF.
//!
//! Keyed by an arbitrary string (e.g. `wrapped-ip:1.2.3.4`). Fixed window rather than a token bucket
//! for simplicity: within each `window_secs` slice a key may be seen up to `limit` times.
//!
//! Design notes, both aimed at not letting the limiter become its own DoS:
//! - Rollover is O(1) amortized. State is one map plus the current window id; when the window changes
//!   the map is cleared once (fresh budgets), so the hot path never scans the whole table under the
//!   lock. Counting is a single hashmap lookup.
//! - Memory is bounded by eviction, not fail-open. Past [`MAX_KEYS`] a new key evicts one existing
//!   entry rather than being dropped. Failing open would let a distinct-key flood silently disable the
//!   limiter; failing closed would lock out everyone. Eviction just resets the evicted key's window
//!   count, so it can neither disable limiting nor lock out a legitimate caller.
//!
//! NOTE on the IP key (set by the caller, not here): behind a reverse proxy the socket peer is the
//! proxy, so all clients share one bucket and operators must rate-limit at the edge. We do not trust
//! `X-Forwarded-For` (client-settable: an attacker could forge distinct IPs to evade, or a victim's to
//! lock them out). IPv6 is collapsed to a /64 by the caller so one host can't mint unlimited buckets.

use std::collections::HashMap;
use std::sync::Mutex;

/// Max distinct keys tracked at once. Past this, inserting a new key evicts one existing entry, so the
/// table can't grow without bound. Sized well above any legitimate concurrent caller set.
const MAX_KEYS: usize = 50_000;

#[derive(Default)]
struct Inner {
    window_id: u64,
    counts: HashMap<String, u32>,
}

#[derive(Default)]
pub struct RateLimiter {
    inner: Mutex<Inner>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one hit for `key` and return whether it is still within `limit` for the current
    /// `window_secs` window. `now_unix` is passed in so callers stay testable.
    pub fn check(&self, key: &str, limit: u32, window_secs: u64, now_unix: u64) -> bool {
        let window = now_unix / window_secs.max(1);
        let mut g = self.inner.lock().unwrap();
        if g.window_id != window {
            // Fixed-window rollover: everyone gets a fresh budget. One clear per window, not per call.
            g.window_id = window;
            g.counts.clear();
        }
        if let Some(c) = g.counts.get_mut(key) {
            *c = c.saturating_add(1);
            return *c <= limit;
        }
        // Unseen key this window. Bound memory by eviction (see module doc) before inserting.
        if g.counts.len() >= MAX_KEYS {
            if let Some(victim) = g.counts.keys().next().cloned() {
                g.counts.remove(&victim);
            }
        }
        g.counts.insert(key.to_string(), 1);
        limit >= 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_limit_then_blocks_within_a_window() {
        let rl = RateLimiter::new();
        // limit 3 per 60s window, all at the same instant
        assert!(rl.check("ip:alice", 3, 60, 1000));
        assert!(rl.check("ip:alice", 3, 60, 1000));
        assert!(rl.check("ip:alice", 3, 60, 1000));
        assert!(!rl.check("ip:alice", 3, 60, 1000), "4th hit in the window is blocked");
        // a different key has its own budget
        assert!(rl.check("ip:bob", 3, 60, 1000));
    }

    #[test]
    fn resets_in_the_next_window() {
        let rl = RateLimiter::new();
        assert!(rl.check("k", 1, 60, 1000));
        assert!(!rl.check("k", 1, 60, 1000), "blocked in window 16 (1000/60)");
        // 60s later → next window → budget refreshes
        assert!(rl.check("k", 1, 60, 1060));
    }

    #[test]
    fn clears_the_map_on_window_rollover() {
        let rl = RateLimiter::new();
        rl.check("a", 5, 60, 1000);
        rl.check("b", 5, 60, 1000);
        assert_eq!(rl.inner.lock().unwrap().counts.len(), 2);
        // a far-future check is a new window → the whole table resets (no per-request scan needed)
        rl.check("c", 5, 60, 100_000);
        assert_eq!(rl.inner.lock().unwrap().counts.len(), 1, "rollover cleared the previous window");
    }

    #[test]
    fn map_is_bounded_by_eviction_at_capacity() {
        let rl = RateLimiter::new();
        // Fill to capacity with distinct keys in one window.
        for i in 0..MAX_KEYS {
            assert!(rl.check(&format!("k{i}"), 5, 60, 1000));
        }
        assert_eq!(rl.inner.lock().unwrap().counts.len(), MAX_KEYS);
        // A new key past capacity evicts one entry and is itself tracked: memory stays capped AND
        // limiting keeps working (not fail-open).
        assert!(rl.check("newcomer", 1, 60, 1000), "first hit allowed");
        assert_eq!(rl.inner.lock().unwrap().counts.len(), MAX_KEYS, "table stays at the cap");
        assert!(!rl.check("newcomer", 1, 60, 1000), "newcomer is tracked → 2nd hit blocked, not fail-open");
    }
}
