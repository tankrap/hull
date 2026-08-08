//! A tiny in-memory fixed-window rate limiter. Used to throttle the unauthenticated
//! `sovereign/wrapped` endpoint (bulk enumeration + mass harvesting of encrypted key bundles); the
//! per-account offline-attack resistance still rests on the client Argon2id KDF.
//!
//! Keyed by an arbitrary string (e.g. a username, or a global bucket). Fixed window rather than a
//! token bucket for simplicity: within each `window_secs` slice a key may be seen up to `limit` times.
//! The map is pruned to the current and previous window on each check, so it can't grow without bound
//! even under many distinct keys.
//!
//! NOTE: this is not per-IP — the server isn't wired with connect-info, and behind a proxy a client IP
//! would be the proxy anyway. Per-key + global caps bound automated abuse without that dependency.

use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
pub struct RateLimiter {
    // key -> (window id, count in that window)
    inner: Mutex<HashMap<String, (u64, u32)>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one hit for `key` and return whether it is still within `limit` for the current
    /// `window_secs` window. `now_unix` is passed in so callers stay testable.
    pub fn check(&self, key: &str, limit: u32, window_secs: u64, now_unix: u64) -> bool {
        let window = now_unix / window_secs.max(1);
        let mut m = self.inner.lock().unwrap();
        // prune anything older than the previous window so the map stays bounded.
        m.retain(|_, (w, _)| *w + 1 >= window);
        let entry = m.entry(key.to_string()).or_insert((window, 0));
        if entry.0 != window {
            *entry = (window, 0);
        }
        entry.1 += 1;
        entry.1 <= limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_limit_then_blocks_within_a_window() {
        let rl = RateLimiter::new();
        // limit 3 per 60s window, all at the same instant
        assert!(rl.check("u:alice", 3, 60, 1000));
        assert!(rl.check("u:alice", 3, 60, 1000));
        assert!(rl.check("u:alice", 3, 60, 1000));
        assert!(!rl.check("u:alice", 3, 60, 1000), "4th hit in the window is blocked");
        // a different key has its own budget
        assert!(rl.check("u:bob", 3, 60, 1000));
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
    fn prunes_stale_keys() {
        let rl = RateLimiter::new();
        rl.check("old", 5, 60, 1000);
        // far-future check prunes the stale "old" entry (its window is >1 behind)
        rl.check("new", 5, 60, 100_000);
        assert_eq!(rl.inner.lock().unwrap().len(), 1, "only the current-window key remains");
    }
}
