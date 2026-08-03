//! Per-account forge connections. An org admin **explicitly** connects their account to a forge
//! (today: a GitHub App installation) before anything can be imported — so GitHub repos are never
//! visible or importable without a deliberate, verified connection tied to an account they administer.
//! Keyed by account id; a self-contained JSON side-store (same pattern as autonomy / CI / settings).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Connection {
    /// Forge provider, e.g. "github".
    pub provider: String,
    /// Opaque connection handle the mirror understands (a GitHub App installation id).
    pub installation: String,
    /// The external account login this connection grants access to (for display).
    pub login: String,
    #[serde(default)]
    pub connected_unix: u64,
}

pub struct ForgeConnections {
    path: PathBuf,
    inner: Mutex<HashMap<String, Connection>>,
}

impl ForgeConnections {
    pub fn from_env() -> Self {
        let path = std::env::var("HULL_CONNECTIONS").map(PathBuf::from).unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(format!("{home}/.hull/connections.json"))
        });
        let inner = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        ForgeConnections { path, inner: Mutex::new(inner) }
    }

    pub fn get(&self, account: &str) -> Option<Connection> {
        self.inner.lock().unwrap().get(account).cloned()
    }

    pub fn set(&self, account: &str, conn: Connection) {
        let mut g = self.inner.lock().unwrap();
        g.insert(account.to_string(), conn);
        self.persist(&g);
    }

    pub fn remove(&self, account: &str) {
        let mut g = self.inner.lock().unwrap();
        g.remove(account);
        self.persist(&g);
    }

    fn persist(&self, map: &HashMap<String, Connection>) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(bytes) = serde_json::to_string_pretty(map) {
            let _ = std::fs::write(&self.path, bytes);
        }
    }
}
