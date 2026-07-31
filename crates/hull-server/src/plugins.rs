//! Assembling the plugin [`Registry`]: core built-ins first (so the OSS server is fully functional
//! with no plugins), then any feature-gated plugins. **This function is the open-core seam** — the
//! hosted build adds one line behind its own feature to register private plugins.

use hull_plugin::{AuthProvider, NotifyEvent, Notifier, Plugin, Registry};
use std::sync::Arc;

/// Build the registry the server runs with.
pub fn build_registry() -> Registry {
    let mut reg = Registry::new();

    // Core built-ins — always present, keep the OSS core self-sufficient.
    reg.install(&CorePlugin);

    // Reference example plugin (feature `example-plugins`). The HOSTED build instead enables a
    // `hosted` feature that calls `hull_hosted::register(&mut reg)` — same shape, private crate.
    #[cfg(feature = "example-plugins")]
    hull_plugin_example::register(&mut reg);

    reg
}

/// The always-on core capabilities. The built-in secret engine is already merged by the registry;
/// here we add a logging notifier and a keypair auth stub so `notify`/`authenticate` work out of
/// the box.
struct CorePlugin;

impl Plugin for CorePlugin {
    fn name(&self) -> &str {
        "core"
    }
    fn description(&self) -> &str {
        "built-in capabilities (log notifier, keypair auth); the OSS baseline"
    }
    fn register(&self, reg: &mut Registry) {
        reg.add_notifier(Arc::new(LogNotifier));
        reg.add_auth_provider(Arc::new(KeypairAuth));
    }
}

/// Logs notifications to stderr. Hosted plugins deliver over real channels.
struct LogNotifier;
impl Notifier for LogNotifier {
    fn notify(&self, event: &NotifyEvent) {
        eprintln!("notify[{}] to={:?}: {}", event.kind, event.to, event.summary);
    }
}

/// Placeholder keypair auth: `actor:<id>` credentials resolve to that actor. M1 replaces this with
/// real Ed25519 signature verification.
struct KeypairAuth;
impl AuthProvider for KeypairAuth {
    fn authenticate(&self, credential: &str) -> Option<String> {
        credential.strip_prefix("actor:").map(str::to_string)
    }
}
