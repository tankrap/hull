//! Assembling the plugin [`Registry`]: core built-ins first (so the OSS server is fully functional
//! with no plugins), then any feature-gated plugins. **This function is the open-core seam** — the
//! hosted build adds one line behind its own feature to register private plugins.

use hull_plugin::{AuthProvider, DotenvConfig, EnvConfig, FileSecretConfig, NotifyEvent, Notifier, Plugin, Registry};
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
        // Config/secret resolution, tried in this order: process env, then a file-secret whose path
        // is given by env (HULL_SECRET_FILE_<KEY> — e.g. ~/.openrouter), then a dotenv file. A hosted
        // plugin adds Infisical/Vault ahead of these.
        reg.add_config_provider(Arc::new(EnvConfig));
        reg.add_config_provider(Arc::new(FileSecretConfig));
        reg.add_config_provider(Arc::new(DotenvConfig::load()));
    }
}

/// Logs notifications to stderr. Hosted plugins deliver over real channels.
struct LogNotifier;
#[async_trait::async_trait]
impl Notifier for LogNotifier {
    async fn notify(&self, event: &NotifyEvent) {
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
