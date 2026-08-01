//! Hull's plugin SDK — the **open-core seam**.
//!
//! The whole Hull server + this SDK are Apache-2.0 and fully functional on their own. Extra value
//! for the hosted product (managed auth, autoscaled CI, billing, enterprise notifications, managed
//! secret-rule feeds, …) ships as **closed plugins** that implement the traits here and register at
//! build time. The OSS core never depends on those plugins — it only depends on this SDK — so the
//! core can be given away while the hosted plugins stay private. See `PLUGINS.md`.
//!
//! Model: a [`Plugin`] contributes zero or more **capabilities** to a [`Registry`]. The server asks
//! the registry for the active provider of each capability and falls back to a built-in default, so
//! a server with no plugins still works. Capabilities are trait objects, so a plugin can be a crate
//! compiled in (first-party hosted) — and a WASM/out-of-process host is a future capability kind.

use hull_scan::Finding;
use std::sync::Arc;

/// A unit of extension. A plugin names itself and registers its capabilities.
pub trait Plugin: Send + Sync {
    /// Stable id, e.g. `hosted-sso` or `example`.
    fn name(&self) -> &str;
    /// One-line description shown in `/api/plugins`.
    fn description(&self) -> &str {
        ""
    }
    /// Contribute capabilities. Called once at startup with the registry being assembled.
    fn register(&self, reg: &mut Registry);
}

// ── capability traits (the stable extension points) ────────────────────────────────────────────

/// Extra secret-scanning rules layered on top of the built-in [`hull_scan`] engine. Hosted ships a
/// managed, frequently-updated ruleset; the core has its built-in rules. The server runs both.
pub trait SecretRuleset: Send + Sync {
    fn extra_findings(&self, text: &str) -> Vec<Finding>;
}

/// Deliver a notification (code-owner pings, review requests, CI results). Core default logs;
/// hosted plugins deliver over email/Slack/nostr/managed fan-out.
pub trait Notifier: Send + Sync {
    fn notify(&self, event: &NotifyEvent);
}

/// Authenticate a request into an actor id. Core default verifies a keypair signature; hosted
/// plugins add SSO/SAML/OAuth. Returning `None` means "not my scheme — try the next provider".
pub trait AuthProvider: Send + Sync {
    fn authenticate(&self, credential: &str) -> Option<String>;
}

/// Run a change's checks and return a verdict (the reviewer/CI runtime, D1/D2). The core default
/// [`default_local_ci`] checks out the change and runs its detected test command locally; the hosted
/// plugin runs it on autoscaled, content-addressed runners. Either way it produces the green/red
/// signal that keel verification — and the reconciliation ledger — consume.
pub trait CiRunner: Send + Sync {
    fn run(&self, req: &CiRequest) -> CiOutcome;
}

/// A checkout of the change to run checks against. The core plumbing resolves the change to its keel
/// tree and materializes it at `workdir` before calling the runner; `tree_id` is the content address
/// that makes results memoizable (same tree ⇒ same verdict).
#[derive(Debug, Clone)]
pub struct CiRequest {
    pub repo: String,
    pub change: String,
    pub tree_id: String,
    pub workdir: std::path::PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CiStatus {
    Green,
    Red,
    /// The runner could not produce a verdict (no test command, timeout, spawn failure).
    Errored,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CiOutcome {
    pub status: CiStatus,
    pub summary: String,
    /// Set by the core plumbing when this result was served from the content-addressed memo rather
    /// than a fresh run.
    pub memoized: bool,
}

/// The built-in local runner: detect the repo's test command from the checked-out tree and run it
/// with a wall-clock bound. `Cargo.toml → cargo test`, `package.json → npm test --silent`, else no
/// runnable checks. Honors a `HULL_CI_CMD` override (run via `sh -c`) for repos with a custom command.
pub fn default_local_ci(req: &CiRequest) -> CiOutcome {
    use std::process::Command;
    let dir = &req.workdir;
    let (program, args): (String, Vec<String>) = if let Ok(cmd) = std::env::var("HULL_CI_CMD") {
        ("sh".into(), vec!["-c".into(), cmd])
    } else if dir.join("Cargo.toml").is_file() {
        ("cargo".into(), vec!["test".into(), "--quiet".into()])
    } else if dir.join("package.json").is_file() {
        ("npm".into(), vec!["test".into(), "--silent".into()])
    } else {
        return CiOutcome {
            status: CiStatus::Errored,
            summary: "no test command detected (no Cargo.toml/package.json, no HULL_CI_CMD)".into(),
            memoized: false,
        };
    };
    let timeout_secs: u64 = std::env::var("HULL_CI_TIMEOUT").ok().and_then(|s| s.parse().ok()).unwrap_or(600);
    let mut child = match Command::new(&program).args(&args).current_dir(dir).spawn() {
        Ok(c) => c,
        Err(e) => {
            return CiOutcome { status: CiStatus::Errored, summary: format!("could not spawn {program}: {e}"), memoized: false };
        }
    };
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let cmd = format!("{program} {}", args.join(" "));
                return if status.success() {
                    CiOutcome { status: CiStatus::Green, summary: format!("`{cmd}` passed"), memoized: false }
                } else {
                    CiOutcome { status: CiStatus::Red, summary: format!("`{cmd}` failed ({status})"), memoized: false }
                };
            }
            Ok(None) => {
                if start.elapsed().as_secs() >= timeout_secs {
                    let _ = child.kill();
                    return CiOutcome { status: CiStatus::Errored, summary: format!("timed out after {timeout_secs}s"), memoized: false };
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return CiOutcome { status: CiStatus::Errored, summary: format!("wait failed: {e}"), memoized: false },
        }
    }
}

/// A notification payload (kept generic so plugins map it to their channel).
#[derive(Debug, Clone)]
pub struct NotifyEvent {
    /// e.g. `code_owner_referenced`, `review_requested`, `ci_failed`.
    pub kind: String,
    /// Target actor ids.
    pub to: Vec<String>,
    /// Human-readable summary.
    pub summary: String,
    /// Optional keel change id this is about (so an agent recipient can act).
    pub change: Option<String>,
}

// ── registry ───────────────────────────────────────────────────────────────────────────────────

/// The assembled capability set. Built with core defaults, then each plugin layers on. The server
/// reads capabilities through the getters, which always return something usable.
#[derive(Default)]
pub struct Registry {
    plugins: Vec<PluginInfo>,
    secret_rulesets: Vec<Arc<dyn SecretRuleset>>,
    notifiers: Vec<Arc<dyn Notifier>>,
    auth_providers: Vec<Arc<dyn AuthProvider>>,
    ci_runner: Option<Arc<dyn CiRunner>>,
}

/// What `/api/plugins` reports.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PluginInfo {
    pub name: String,
    pub description: String,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Run a plugin's registration.
    pub fn install(&mut self, plugin: &dyn Plugin) {
        self.plugins.push(PluginInfo { name: plugin.name().to_string(), description: plugin.description().to_string() });
        plugin.register(self);
    }

    // Capability contributions (called from `Plugin::register`).
    pub fn add_secret_ruleset(&mut self, r: Arc<dyn SecretRuleset>) {
        self.secret_rulesets.push(r);
    }
    pub fn add_notifier(&mut self, n: Arc<dyn Notifier>) {
        self.notifiers.push(n);
    }
    pub fn add_auth_provider(&mut self, a: Arc<dyn AuthProvider>) {
        self.auth_providers.push(a);
    }
    /// Install a CI runner (last one installed wins — a hosted plugin overrides the local default).
    pub fn set_ci_runner(&mut self, r: Arc<dyn CiRunner>) {
        self.ci_runner = Some(r);
    }

    /// Installed plugins (for `/api/plugins`).
    pub fn plugins(&self) -> &[PluginInfo] {
        &self.plugins
    }

    /// All secret findings: the built-in engine plus every plugin ruleset (deduped by fingerprint).
    pub fn scan_secrets(&self, text: &str) -> Vec<Finding> {
        let mut out = hull_scan::scan(text);
        for r in &self.secret_rulesets {
            out.extend(r.extra_findings(text));
        }
        out.sort_by(|a, b| (a.line, a.column, &a.rule).cmp(&(b.line, b.column, &b.rule)));
        out.dedup_by(|a, b| a.fingerprint == b.fingerprint && a.rule == b.rule);
        out
    }

    /// Fan a notification out to every registered notifier.
    pub fn notify(&self, event: &NotifyEvent) {
        for n in &self.notifiers {
            n.notify(event);
        }
    }

    /// First auth provider that recognizes the credential.
    pub fn authenticate(&self, credential: &str) -> Option<String> {
        self.auth_providers.iter().find_map(|a| a.authenticate(credential))
    }

    /// Run a change's checks: the installed CI runner if any, else the built-in local runner.
    pub fn run_ci(&self, req: &CiRequest) -> CiOutcome {
        match &self.ci_runner {
            Some(r) => r.run(req),
            None => default_local_ci(req),
        }
    }
}
