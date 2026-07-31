//! Assembling the plugin [`Registry`]: core built-ins first (so the OSS server is fully functional
//! with no plugins), then any feature-gated plugins. **This function is the open-core seam** — the
//! hosted build adds one line behind its own feature to register private plugins.

use hull_plugin::{AuthProvider, NotifyEvent, Notifier, Plugin, Registry};
use std::sync::Arc;

/// Build the registry: core built-ins first (so the OSS core is self-sufficient), then whatever
/// `register_extra` adds. The OSS binary passes a no-op; a private hosted binary passes a closure
/// that registers its closed plugins — the core never names them.
pub fn build_registry(register_extra: impl FnOnce(&mut Registry)) -> Registry {
    let mut reg = Registry::new();
    reg.install(&CorePlugin);
    register_extra(&mut reg);
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
