//! A tiny in-memory fixed-window rate limiter. Used to throttle the unauthenticated
//! `sovereign/wrapped` endpoint (per client IP and per username); the per-account offline-attack
//! resistance still rests on the client Argon2id KDF.
//!
//! Keyed by an arbitrary string (e.g. `wrapped-ip:1.2.3.4` or `wrapped-user:alice`). Fixed window
//! rather than a token bucket for simplicity: within each `window_secs` slice a key may be seen up to
//! `limit` times.
//!
//! Two things keep memory bounded even under a flood of distinct keys (many attacker-chosen usernames
//! or spoofed source IPs): every check prunes entries older than the previous window, and the map has a
//! hard key cap. When the map is at capacity a brand-new key is ALLOWED without being inserted (fail
//! open on limiting, never on memory). That is deliberate: an already-tracked caller stays limited, and
//! we never lock out an unseen caller just because the table filled — availability wins on this path.
//!
//! NOTE: the IP key uses the socket peer address. Behind a reverse proxy that is the proxy's address,
//! so all clients share one bucket; operators in that topology must rate-limit at the edge. We do not
//! trust `X-Forwarded-For`: it is client-settable, so an attacker could forge distinct IPs to evade the
//! per-IP cap or forge a victim's IP to lock them out.

use std::collections::HashMap;
use std::sync::Mutex;

/// Max distinct keys tracked at once. Past this, new keys are allowed but not recorded, so the table
/// can't grow without bound. Sized well above any legitimate concurrent caller set for this endpoint.
const MAX_KEYS: usize = 50_000;

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
    ///
    /// A new key is tracked only when the table is under [`MAX_KEYS`]; at capacity a new key is allowed
    /// without insertion, so a distinct-key flood bounds memory instead of locking out unseen callers.
    pub fn check(&self, key: &str, limit: u32, window_secs: u64, now_unix: u64) -> bool {
        let window = now_unix / window_secs.max(1);
        let mut m = self.inner.lock().unwrap();
        // Prune anything older than the previous window so the map stays bounded across windows.
        m.retain(|_, (w, _)| *w + 1 >= window);
        if let Some(entry) = m.get_mut(key) {
            if entry.0 != window {
                *entry = (window, 0);
            }
            entry.1 += 1;
            entry.1 <= limit
        } else {
            // Unseen key: only start tracking it if there's room, bounding memory under adversarial
            // key churn. When full, allow it (don't insert) rather than deny a caller we can't record.
            if m.len() >= MAX_KEYS {
                return true;
            }
            m.insert(key.to_string(), (window, 1));
            1 <= limit
        }
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

    #[test]
    fn map_is_bounded_and_fails_open_when_full() {
        let rl = RateLimiter::new();
        // Fill to capacity with distinct keys in one window (limit 1 each).
        for i in 0..MAX_KEYS {
            assert!(rl.check(&format!("k{i}"), 1, 60, 1000));
        }
        assert_eq!(rl.inner.lock().unwrap().len(), MAX_KEYS);
        // A brand-new key past capacity is allowed but NOT recorded: memory can't grow further.
        assert!(rl.check("overflow", 1, 60, 1000), "new key at capacity fails open");
        assert_eq!(rl.inner.lock().unwrap().len(), MAX_KEYS, "table did not grow past the cap");
        // An already-tracked key is still limited even when the table is full.
        assert!(!rl.check("k0", 1, 60, 1000), "existing key stays limited at capacity");
    }
}
