//! Shared load/persist plumbing for the small JSON side-stores (`~/.hull/*.json`).
//!
//! Every side-store here follows the same shape: an env-var-or-`~/.hull/<x>.json` path, a lenient
//! load (`read_to_string` → `from_str` → `unwrap_or_default`, so a missing/garbage file starts
//! empty rather than crashing the server), and a pretty-printed persist.
//!
//! The persist is the part that matters for durability: [`persist_json_atomic`] writes to a temp
//! file in the **same directory** and `rename`s it over the target, so a crash mid-write can never
//! truncate or corrupt the live file (the lenient load would otherwise silently discard it). This is
//! the same temp-file + rename dance the durable [`crate::store::FileStore`] and
//! [`crate::activity`]/[`crate::agentsession`] stores already use.
//!
//! The serialized bytes are identical to a bare `to_string_pretty` + `write` — only the write
//! mechanism changes.

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::Path;

/// Lenient load of a JSON side-store: a missing or unparseable file yields `T::default()` rather
/// than an error, matching every store's `read_to_string.ok().and_then(from_str).unwrap_or_default()`.
pub fn load_json<T: DeserializeOwned + Default>(path: &Path) -> T {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Persist a JSON side-store **atomically**: pretty-print, write to `<path>.json.tmp` in the same
/// directory, then `rename` it over the target (an atomic replace on the same filesystem). A crash
/// mid-write leaves the previous good file intact instead of a half-written one. Parent directories
/// are created as needed. Errors are swallowed, matching the callers' best-effort `let _ = …` writes.
pub fn persist_json_atomic<T: Serialize>(path: &Path, value: &T) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(value) {
        let tmp = path.with_extension("json.tmp");
        let _ = std::fs::write(&tmp, json).and_then(|_| std::fs::rename(&tmp, path));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn persist_then_load_round_trips_and_is_atomic() {
        let path = std::env::temp_dir().join(format!("hull-jsonstore-test-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);

        // Missing file loads as default (empty map), never an error.
        let empty: HashMap<String, u32> = load_json(&path);
        assert!(empty.is_empty());

        let mut m = HashMap::new();
        m.insert("a".to_string(), 1u32);
        m.insert("b".to_string(), 2u32);
        persist_json_atomic(&path, &m);

        // Round-trips to the same value…
        let back: HashMap<String, u32> = load_json(&path);
        assert_eq!(back, m);

        // …with byte-identical content to a plain pretty write (format unchanged), and no leftover
        // temp file (the rename consumed it).
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, serde_json::to_string_pretty(&m).unwrap());
        assert!(!path.with_extension("json.tmp").exists(), "temp file is renamed away, not left behind");

        let _ = std::fs::remove_file(&path);
    }
}
