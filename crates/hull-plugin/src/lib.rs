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
/// hosted plugins deliver over email/Slack/nostr/managed fan-out. `async` (via [`async_trait`]) so a
/// notifier can `.await` the store / a channel client; `Arc<dyn Notifier>` needs `#[async_trait]`
/// (native async-fn-in-trait isn't dyn-safe here).
#[async_trait::async_trait]
pub trait Notifier: Send + Sync {
    async fn notify(&self, event: &NotifyEvent);
}

/// Authenticate a request into an actor id. Core default verifies a keypair signature; hosted
/// plugins add SSO/SAML/OAuth. Returning `None` means "not my scheme — try the next provider".
pub trait AuthProvider: Send + Sync {
    fn authenticate(&self, credential: &str) -> Option<String>;
}

// ── config / secrets ────────────────────────────────────────────────────────────────────────────

/// Resolve a config value or secret by key (e.g. `OPENROUTER_API_KEY`). The core ships
/// env / file-secret / dotenv providers; a hosted plugin adds Infisical, Vault, AWS/GCP secret
/// managers, etc. — **no provider hardcodes a path or a secret**. Providers are tried in order; the
/// first non-empty value wins. `None` means "not mine — try the next provider".
pub trait ConfigProvider: Send + Sync {
    fn name(&self) -> &str;
    fn get(&self, key: &str) -> Option<String>;
}

/// Process environment variables (`FOO` → `std::env::var("FOO")`).
pub struct EnvConfig;
impl ConfigProvider for EnvConfig {
    fn name(&self) -> &str {
        "env"
    }
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.is_empty())
    }
}

/// A secret whose VALUE lives in a file, with the **path supplied by env**
/// `HULL_SECRET_FILE_<KEY>` — so e.g. `~/.openrouter` is configured at deploy time, never baked into
/// the app. The file's trimmed contents are the value. A leading `~/` expands to `$HOME`.
pub struct FileSecretConfig;
impl ConfigProvider for FileSecretConfig {
    fn name(&self) -> &str {
        "file-secret"
    }
    fn get(&self, key: &str) -> Option<String> {
        let raw = std::env::var(format!("HULL_SECRET_FILE_{key}")).ok().filter(|p| !p.is_empty())?;
        let path = match raw.strip_prefix("~/") {
            Some(rest) => format!("{}/{rest}", std::env::var("HOME").unwrap_or_default()),
            None => raw,
        };
        std::fs::read_to_string(path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    }
}

/// A dotenv file of `KEY=VALUE` lines. Path from `HULL_ENV_FILE` (default `.env`); `#` comments and
/// surrounding quotes are handled. Loaded once at startup.
pub struct DotenvConfig {
    map: std::collections::HashMap<String, String>,
}
impl DotenvConfig {
    pub fn load() -> Self {
        let path = std::env::var("HULL_ENV_FILE").unwrap_or_else(|_| ".env".into());
        let mut map = std::collections::HashMap::new();
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                let l = line.trim();
                if l.is_empty() || l.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = l.split_once('=') {
                    map.insert(k.trim().to_string(), v.trim().trim_matches('"').to_string());
                }
            }
        }
        DotenvConfig { map }
    }
}
impl ConfigProvider for DotenvConfig {
    fn name(&self) -> &str {
        "dotenv"
    }
    fn get(&self, key: &str) -> Option<String> {
        self.map.get(key).cloned().filter(|v| !v.is_empty())
    }
}

/// Mirror a landed change to an external forge (GitHub is the first target — the top adoption lever:
/// a team keeps its GitHub while Hull hosts the agent-native layer). The core default
/// [`LogMirror`] is a dry-run that records what it *would* push (so the loop-prevention and
/// idempotency plumbing is testable without credentials); the hosted plugin pushes for real via a
/// GitHub App installation. Direction and origin tracking live in the core plumbing, not here — a
/// [`Mirror`] only has to know how to push one change outbound.
pub trait Mirror: Send + Sync {
    /// The external target for `repo`, e.g. `github:tankrap/hull`, or `None` if this repo isn't
    /// linked. Used for the idempotency key and the UI's mirror status.
    fn target(&self, repo: &str) -> Option<String>;
    /// Push one landed change outbound. Returns an external ref (a sha / PR url) on success.
    fn push(&self, req: &MirrorPush) -> MirrorResult;
    /// Pull the forge's current state for `repo` **into** hull (the inbound half of a two-way mirror):
    /// fetch the forge's git and ingest it into hull's keel store. Called by the inbound webhook after
    /// signature + loop checks pass. Default: unsupported (a one-way/dry-run mirror does nothing).
    fn pull_in(&self, _repo: &str) -> MirrorResult {
        MirrorResult { ok: false, external_ref: None, detail: "inbound pull not supported by this mirror".into() }
    }
    /// Verify a forge **connection** (e.g. a GitHub App installation id) and return the external
    /// account login it grants access to, or `None` if it's invalid / unsupported. Used to let an org
    /// admin explicitly connect their account to a forge before any import — so nothing is ever
    /// importable without a deliberate, verified connection.
    fn verify_connection(&self, _connection: &str) -> Option<String> {
        None
    }
    /// External repos importable through `connection` (forge full-names). Default: none.
    fn list_importable(&self, _connection: &str) -> Vec<String> {
        Vec::new()
    }
    /// Every connection this forge already grants (e.g. GitHub App installations) as
    /// `(connection_id, external_login)` — so an admin can PICK their org instead of pasting an id.
    /// Default: none (a mirror with no discoverable connections).
    fn list_installations(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    /// Import `source` (a forge full-name) through `connection` INTO hull's `dest` (`tenant/repo`).
    /// Default: unsupported.
    fn import_repo(&self, _connection: &str, _source: &str, _dest: &str) -> MirrorResult {
        MirrorResult { ok: false, external_ref: None, detail: "import not supported by this mirror".into() }
    }
}

#[derive(Debug, Clone)]
pub struct MirrorPush {
    pub repo: String,
    pub change: String,
    pub intent: String,
    pub author: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MirrorResult {
    pub ok: bool,
    /// The external reference created (e.g. a GitHub commit sha or PR url), when known.
    pub external_ref: Option<String>,
    pub detail: String,
}

/// The core default mirror: a dry-run that logs the push and returns a synthetic ref, so the
/// change-land → outbound-push path (and its loop prevention) works end-to-end without a real
/// forge. Enabled by setting `HULL_MIRROR_TARGET` (e.g. `github:tankrap/hull`); unset means no repo
/// is linked and nothing is mirrored.
pub struct LogMirror;

impl Mirror for LogMirror {
    fn target(&self, repo: &str) -> Option<String> {
        // `HULL_MIRROR_TARGET` (e.g. "github:tenant/repo") configures ONE repo's mirror — only that
        // repo reports a target, not every repo in the instance.
        let t = std::env::var("HULL_MIRROR_TARGET").ok().filter(|s| !s.is_empty())?;
        let repo_part = t.split_once(':').map(|(_, r)| r).unwrap_or(&t);
        (repo_part == repo).then_some(t)
    }
    fn push(&self, req: &MirrorPush) -> MirrorResult {
        let target = std::env::var("HULL_MIRROR_TARGET").unwrap_or_default();
        eprintln!("hull-mirror(dry-run): would push {} @ {} → {target}", req.repo, &req.change[..req.change.len().min(12)]);
        // A deterministic synthetic external ref (no RNG in this runtime): the change id is already
        // the content address, so echo a short form as the stand-in for a forge sha.
        MirrorResult { ok: true, external_ref: Some(format!("dry:{}", &req.change[..req.change.len().min(12)])), detail: format!("dry-run push to {target}") }
    }
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

// ── Fixer (review → fix loop) ────────────────────────────────────────────────────────────────────

/// Propose a fix for a review finding. Given the finding (file + note) and the keel-native source, a
/// model produces a concrete patch. Like [`Reviewer`], the core has no default (fixing needs a
/// model); the hosted plugin drives one. The result is *proposed* — Hull posts it for a human, or at
/// T3 the agent acts on it — never silently rewriting code without the merge gate + CI catching a
/// bad fix.
pub trait Fixer: Send + Sync {
    fn fix(&self, req: &FixRequest) -> FixResult;
}

/// An AI backend resolved for one request — the provider + base URL + bearer token the hosted reviewer
/// should call, chosen (and rotated) by the core from the owning account/org's connected credentials.
/// `None` on a request means "use the process-configured provider" (the `.env` default).
#[derive(Debug, Clone)]
pub struct AiCredential {
    /// API provider (`openai`/`anthropic`/`openrouter`) OR an agent kind (`claude-code`/`codex`).
    pub provider: String,
    pub base_url: String,
    /// The bearer token (an API key). Empty when this is an agent CLI.
    pub token: String,
    /// When set, this backend is a locally-installed **agent CLI** (the string is the command, e.g.
    /// `claude`), run with the user's own subscription login — not an API call.
    pub agent_cli: Option<String>,
    /// For a per-user agent session, the credential-bundle directory to point the CLI at (its
    /// `CLAUDE_CONFIG_DIR` / `CODEX_HOME`). `None` ⇒ the agent uses the Hull host's own login.
    pub agent_config_dir: Option<String>,
    /// The owning [`AiConnection`] id, so Hull can meter this run's token usage against it.
    pub connection_id: Option<String>,
}

/// Token usage reported by an agent run (parsed from the CLI's own accounting), for per-connection
/// metering. `cost_micros` is USD × 1e6 when the agent reports a cost.
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_micros: u64,
}

#[derive(Debug, Clone)]
pub struct FixRequest {
    pub repo: String,
    pub change: String,
    /// keel-native content-addressed source (the tree tar) — the fixer fetches the file to patch.
    pub source_url: String,
    pub path: String,
    pub note: String,
    pub severity: String,
    /// Resolved AI backend for this request (from the owner's connections), or None for the default.
    pub ai_credential: Option<AiCredential>,
}

/// One edit as a **search/replace** on a file — `search` is the exact existing code, `replace` is
/// the corrected code. This applies deterministically (find the verbatim `search`, swap it), so a
/// model fix can be materialized as a real keel change without guessing diff line numbers.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FixEdit {
    pub path: String,
    pub search: String,
    pub replace: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FixResult {
    pub ok: bool,
    /// One-line explanation of the fix.
    pub explanation: String,
    /// The edits to apply (empty if `ok` is false).
    pub edits: Vec<FixEdit>,
    /// Token usage for this fix, when the backend reported it (agent-CLI path). Internal metering.
    #[serde(skip)]
    pub usage: Option<Usage>,
}

// ── Reviewer (Epic D / D1) ──────────────────────────────────────────────────────────────────────

/// Produce a review package for a change (Epic D). The core default [`default_review`] is the
/// read-only **reconciliation** reviewer (Epic C) — deterministic, claims-vs-facts, no model, no
/// code execution. The hosted plugin swaps in the **sandbox + model-backed AI reviewer** (runs the
/// change's code in isolation, writes independent probe tests, model-tiered, and reviews under a
/// model family independent of the author). A `Reviewer`'s verdict is **advisory** input to the
/// merge gate — it never satisfies a protected-path merge on its own (that still needs keel-verify
/// green + an independent human/agent approval).
pub trait Reviewer: Send + Sync {
    fn review(&self, req: &ReviewRequest) -> ReviewPackage;
    /// Answer a reviewer's free-form question about a specific span of code (a line or range). Used by
    /// the "comment & ask agent" action. Default: unsupported (the OSS reconciliation reviewer has no
    /// model to answer with); the hosted AI reviewer overrides it.
    fn answer(&self, _req: &AskRequest) -> Option<String> {
        None
    }
}

/// A reviewer's question about a code span: the human's `question`, the `path`, the `code` around the
/// referenced lines, and the anchor `line`. The reviewer reads the code and replies in prose.
#[derive(Debug, Clone)]
pub struct AskRequest {
    pub repo: String,
    pub path: String,
    pub line: u32,
    pub line_end: u32,
    pub code: String,
    pub question: String,
    pub ai_credential: Option<AiCredential>,
}

/// What a reviewer judges: the change's narrative (intent + session lesson), its author, the
/// keel-native content-addressed source (a sandbox reviewer fetches this — see CI-SPEC §6), and the
/// observed facts (which the reconciliation default reasons over).
#[derive(Debug, Clone)]
pub struct ReviewRequest {
    pub repo: String,
    pub change: String,
    pub intent: String,
    pub lesson: String,
    pub author: String,
    /// The model that *authored* the change (from the keel session), if known — so the reviewer can
    /// pick an independent model family (D5). Empty when the change had no captured session.
    pub author_model: String,
    /// keel-native content-addressed source (a `…/tree/:tree_id/tar` URL). NOT git.
    pub source_url: String,
    pub facts: hull_core::reconcile::ChangeFacts,
    /// Resolved AI backend for this request (from the owner's connections), or None for the default.
    pub ai_credential: Option<AiCredential>,
}

/// The "family" of a model id for independence checks — vendor + line, with the trailing version and
/// `-fast` suffix stripped (`anthropic/claude-sonnet-5` → `anthropic/claude-sonnet`). Two ids share a
/// family iff a reviewer using one would not be meaningfully independent of an author using the other.
pub fn model_family(id: &str) -> String {
    let id = id.trim().to_lowercase();
    let id = id.strip_suffix("-fast").unwrap_or(&id);
    // Drop a trailing version token (digits/dots) after the last '-'.
    match id.rsplit_once('-') {
        Some((head, tail)) if tail.chars().all(|c| c.is_ascii_digit() || c == '.') && !tail.is_empty() => head.to_string(),
        _ => id.to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Approve,
    RequestChanges,
    Comment,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ReviewFinding {
    pub path: String,
    pub line: Option<u32>,
    /// `info` | `warn` | `blocker`.
    pub severity: String,
    pub note: String,
}

/// A reviewer's structured output. **Constrained-schema verdict (D7):** the verdict and findings are
/// structured data produced by the reviewer, never free-text parsed for approval — so untrusted repo
/// content can't talk a reviewer into an approval.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReviewPackage {
    pub verdict: ReviewVerdict,
    pub summary: String,
    pub findings: Vec<ReviewFinding>,
    /// The reconciliation ledger evidence, when the reviewer produced one (the OSS default does; a
    /// model-backed reviewer may attach its own or leave this `None`).
    pub ledger: Option<hull_core::reconcile::ClaimLedger>,
    /// Token usage for this review, when the backend reported it (agent-CLI path). Internal metering.
    #[serde(skip)]
    pub usage: Option<Usage>,
}

/// The OSS default reviewer (Epic C): reconcile the change's narrative against its facts and
/// synthesize a verdict + blocker findings for contradicted claims. Deterministic — no model, no
/// code execution. This is what ships in the free core; hosted swaps in the empirical AI reviewer.
pub fn default_review(req: &ReviewRequest) -> ReviewPackage {
    use hull_core::reconcile::{reconcile, ClaimStatus};
    let ledger = reconcile(&req.change, &req.intent, &req.lesson, &req.facts);
    let anchor = req.facts.files.first().cloned().unwrap_or_else(|| format!("change:{}", &req.change[..req.change.len().min(12)]));
    let mut findings = Vec::new();
    for c in &ledger.claims {
        if c.status == ClaimStatus::Contradicted {
            let ev = c.evidence.iter().find(|e| !e.supports).map(|e| e.detail.clone()).unwrap_or_default();
            findings.push(ReviewFinding {
                path: anchor.clone(),
                line: None,
                severity: "blocker".into(),
                note: format!("Claim not supported by the change: \u{201c}{}\u{201d} — {ev}", c.text),
            });
        }
    }
    let contradicted = ledger.contradicted();
    let verdict = if contradicted > 0 {
        ReviewVerdict::RequestChanges
    } else if req.facts.verification == "green" && ledger.supported() > 0 {
        ReviewVerdict::Approve
    } else {
        ReviewVerdict::Comment
    };
    let summary = format!(
        "Reconciliation review: {} claims — {} supported, {} contradicted; checks {}. {}",
        ledger.claims.len(),
        ledger.supported(),
        contradicted,
        req.facts.verification,
        match verdict {
            ReviewVerdict::Approve => "Narrative matches the change and checks are green.",
            ReviewVerdict::RequestChanges => "The change contradicts its own stated intent.",
            ReviewVerdict::Comment => "Not green, or nothing corroborates the intent — a human should look.",
        }
    );
    ReviewPackage { verdict, summary, findings, ledger: Some(ledger), usage: None }
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
    /// The `"tenant/repo"` key this notification is about, so a recipient UI can link back to it.
    pub repo: Option<String>,
    /// Structured link target within the repo when the notification is about a PR or issue
    /// (`"pr"` / `"issue"`), paired with [`Self::target_number`].
    pub target_kind: Option<String>,
    /// The PR / issue number this notification links to (see [`Self::target_kind`]).
    pub target_number: Option<u64>,
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
    mirror: Option<Arc<dyn Mirror>>,
    reviewer: Option<Arc<dyn Reviewer>>,
    fixer: Option<Arc<dyn Fixer>>,
    config_providers: Vec<Arc<dyn ConfigProvider>>,
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
    /// Install a mirror (a hosted GitHub-App plugin overrides the local dry-run default).
    pub fn set_mirror(&mut self, m: Arc<dyn Mirror>) {
        self.mirror = Some(m);
    }
    /// Install a reviewer (the hosted AI reviewer overrides the OSS reconciliation default).
    pub fn set_reviewer(&mut self, r: Arc<dyn Reviewer>) {
        self.reviewer = Some(r);
    }
    /// Install a fixer (the hosted AI fixer; the core has none).
    pub fn set_fixer(&mut self, f: Arc<dyn Fixer>) {
        self.fixer = Some(f);
    }
    /// Whether an AI fixer / reviewer is installed — so the UI can hide AI actions it can't fulfill.
    pub fn has_fixer(&self) -> bool {
        self.fixer.is_some()
    }
    pub fn has_reviewer(&self) -> bool {
        self.reviewer.is_some()
    }
    /// Add a config/secret provider. Tried in registration order; a hosted plugin adds Infisical/Vault.
    pub fn add_config_provider(&mut self, c: Arc<dyn ConfigProvider>) {
        self.config_providers.push(c);
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
    pub async fn notify(&self, event: &NotifyEvent) {
        for n in &self.notifiers {
            n.notify(event).await;
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

    /// Resolve a config value / secret by key, trying each provider in order. Never logs the value.
    pub fn config(&self, key: &str) -> Option<String> {
        self.config_providers.iter().find_map(|p| p.get(key))
    }

    /// Propose a fix for a finding, if a fixer is installed (`None` in the OSS core).
    pub fn fix(&self, req: &FixRequest) -> Option<FixResult> {
        self.fixer.as_ref().map(|f| f.fix(req))
    }

    /// Produce a review package: the installed reviewer (hosted AI) if any, else the OSS
    /// reconciliation default.
    pub fn review(&self, req: &ReviewRequest) -> ReviewPackage {
        match &self.reviewer {
            Some(r) => r.review(req),
            None => default_review(req),
        }
    }

    /// Ask the installed reviewer a question about a code span. `None` when no reviewer is installed
    /// or it declines to answer.
    pub fn answer(&self, req: &AskRequest) -> Option<String> {
        self.reviewer.as_ref().and_then(|r| r.answer(req))
    }

    /// The external target this repo mirrors to, if any (installed mirror, else the dry-run default).
    pub fn mirror_target(&self, repo: &str) -> Option<String> {
        match &self.mirror {
            Some(m) => m.target(repo),
            None => LogMirror.target(repo),
        }
    }

    /// Push a landed change outbound through the installed mirror (or the dry-run default). The
    /// caller is responsible for loop prevention and idempotency — this only performs the push.
    pub fn mirror_push(&self, req: &MirrorPush) -> MirrorResult {
        match &self.mirror {
            Some(m) => m.push(req),
            None => LogMirror.push(req),
        }
    }
    /// Verify a forge connection through the installed mirror; returns the external account login.
    pub fn mirror_verify_connection(&self, connection: &str) -> Option<String> {
        self.mirror.as_ref().and_then(|m| m.verify_connection(connection))
    }
    /// Import an external repo into hull through the installed mirror + a verified connection.
    pub fn mirror_import(&self, connection: &str, source: &str, dest: &str) -> MirrorResult {
        match &self.mirror {
            Some(m) => m.import_repo(connection, source, dest),
            None => MirrorResult { ok: false, external_ref: None, detail: "no mirror configured".into() },
        }
    }
    /// External repos available to import through the installed mirror + a verified connection.
    pub fn mirror_importable(&self, connection: &str) -> Vec<String> {
        self.mirror.as_ref().map(|m| m.list_importable(connection)).unwrap_or_default()
    }
    /// Discoverable forge connections (e.g. GitHub App installations) as `(id, login)` — for the
    /// "pick your org" connect flow that replaces pasting an installation id.
    pub fn mirror_installations(&self) -> Vec<(String, String)> {
        self.mirror.as_ref().map(|m| m.list_installations()).unwrap_or_default()
    }
    pub fn mirror_pull_in(&self, repo: &str) -> MirrorResult {
        match &self.mirror {
            Some(m) => m.pull_in(repo),
            None => MirrorResult { ok: false, external_ref: None, detail: "no mirror configured".into() },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::model_family;

    #[test]
    fn independence_by_model_family() {
        // Same line, different version → same family (not independent).
        assert_eq!(model_family("anthropic/claude-sonnet-5"), model_family("anthropic/claude-sonnet-4.6"));
        // Different lines → different family (independent).
        assert_ne!(model_family("anthropic/claude-sonnet-5"), model_family("anthropic/claude-opus-4.8"));
        // Different vendors → different family.
        assert_ne!(model_family("anthropic/claude-sonnet-5"), model_family("openai/gpt-4o"));
        // -fast suffix doesn't change the family.
        assert_eq!(model_family("anthropic/claude-opus-4.8-fast"), model_family("anthropic/claude-opus-4.8"));
    }
}
