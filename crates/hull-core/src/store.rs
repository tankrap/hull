//! Storage seam. The scaffold ships an in-memory store; M1+ swaps in a SQL-backed implementation
//! (accounts/issues/projects as relational rows) that references keel objects by content address.
//! Keeping this a trait means the server, tests, and the eventual SQL store all share one shape.

use crate::{Account, Actor, AiConnection, Comment, Issue, OwnerRule, Project, PullRequest, Repo, Review, SessionRecord, Team, User};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use deadpool_postgres::{Config as PgConfig, ManagerConfig, Pool, RecyclingMethod, Runtime};
use tokio_postgres::NoTls;

/// The persistence operations the server needs. Deliberately small for the scaffold — it grows per
/// milestone. All reads clone (cheap at this stage; the SQL store will stream).
///
/// `async` (via [`async_trait`]) so the Postgres backend can `.await` its driver natively inside the
/// server's tokio runtime; the in-memory/file backends do the same synchronous work inside an
/// `async fn`. `Arc<dyn Store>` needs `#[async_trait]` (native async-fn-in-trait isn't dyn-safe here).
#[async_trait::async_trait]
pub trait Store: Send + Sync {
    async fn put_actor(&self, actor: Actor);
    async fn actor(&self, id: &str) -> Option<Actor>;
    async fn actors(&self) -> Vec<Actor>;
    async fn put_account(&self, account: Account);
    async fn accounts(&self) -> Vec<Account>;
    async fn put_repo(&self, repo: Repo);
    async fn repos(&self) -> Vec<Repo>;
    /// Remove a repo record by id. Returns true if one was removed.
    async fn remove_repo(&self, id: &str) -> bool;
    /// Remove every domain record (issues, PRs, reviews, comments, sessions, code-owner rules)
    /// whose repo key is `repo_key` (`<tenant>/<repo>`). Used when a repo is deleted.
    async fn purge_repo_data(&self, repo_key: &str);
    /// Re-key every domain record (issues, PRs, reviews, comments, sessions, code-owner rules)
    /// from `old_key` to `new_key`. Used when a repo is renamed.
    async fn rekey_repo_data(&self, old_key: &str, new_key: &str);
    async fn put_issue(&self, issue: Issue);
    async fn issues(&self, repo: &str) -> Vec<Issue>;
    /// Replace an existing issue, matched by `repo` + `number`. Returns true if one was replaced.
    async fn replace_issue(&self, issue: Issue) -> bool;
    /// Update an issue's title and/or body (and stamp `edited_unix`), matched by `repo` + `number`.
    /// `None` leaves that field unchanged. Returns true if one was updated.
    async fn update_issue_content(&self, repo: &str, number: u64, title: Option<&str>, body: Option<&str>, edited_unix: u64) -> bool;
    async fn put_pr(&self, pr: PullRequest);
    async fn prs(&self, repo: &str) -> Vec<PullRequest>;
    /// Replace an existing PR, matched by `repo` + `number`. Returns true if one was replaced.
    async fn replace_pr(&self, pr: PullRequest) -> bool;
    async fn put_review(&self, review: Review);
    async fn reviews(&self, repo: &str) -> Vec<Review>;
    async fn put_comment(&self, comment: Comment);
    async fn comments(&self, repo: &str) -> Vec<Comment>;
    /// Delete a comment by id. Returns true if one was removed.
    async fn remove_comment(&self, repo: &str, id: &str) -> bool;
    /// Update a comment's body (and stamp `edited_unix`) by id. Returns true if one was updated.
    async fn update_comment_body(&self, repo: &str, id: &str, body: &str, edited_unix: u64) -> bool;
    /// Associate an ingested keel session with a change (latest write wins per change).
    async fn put_session_record(&self, record: SessionRecord);
    async fn session_record(&self, repo: &str, change: &str) -> Option<SessionRecord>;
    async fn put_project(&self, project: Project);
    async fn projects(&self, owner: &str) -> Vec<Project>;
    // ── AI connections (per account/org: multiple backends, optional rotation) ──
    async fn put_ai_connection(&self, conn: AiConnection);
    async fn ai_connections(&self, owner: &str) -> Vec<AiConnection>;
    async fn remove_ai_connection(&self, owner: &str, id: &str) -> bool;
    async fn set_ai_rotate(&self, owner: &str, on: bool);
    async fn ai_rotate(&self, owner: &str) -> bool;
    /// Add one agent run's token usage to a connection's rolling tally.
    async fn add_ai_usage(&self, conn_id: &str, input: u64, output: u64, cost_micros: u64, at_unix: u64);
    /// A connection's accumulated usage (default zeros if none).
    async fn ai_usage(&self, conn_id: &str) -> crate::AiUsage;
    /// Set a repo's code-owner rules (replaces the existing set).
    async fn set_owners(&self, repo: &str, rules: Vec<OwnerRule>);
    async fn owners(&self, repo: &str) -> Vec<OwnerRule>;
    // ── hosted-account identities (passkey login) ──
    async fn put_user(&self, user: User);
    async fn user(&self, id: &str) -> Option<User>;
    async fn user_by_username(&self, username: &str) -> Option<User>;
    async fn user_by_actor(&self, actor: &str) -> Option<User>;
    async fn users(&self) -> Vec<User>;
    // ── org teams ──
    async fn put_team(&self, team: Team);
    async fn team(&self, id: &str) -> Option<Team>;
    async fn teams(&self, account: &str) -> Vec<Team>;
    async fn delete_team(&self, id: &str);
    /// Readiness probe (distinct from liveness): can this backend actually serve traffic right now?
    /// Backends without a remote dependency return `true`; the Postgres backend runs a trivial query
    /// to confirm the pool can hand out a live connection. Infallible by convention (returns `bool`,
    /// never `Result`) like the rest of the trait — a failure surfaces as `false`, which the server's
    /// `/ready` route maps to a 503 so an orchestrator holds traffic off an unready instance.
    async fn ready(&self) -> bool;
}

/// Match a code-owner / protected-path glob against a repo-relative path. Supports:
/// - a leading `**/` segment — `**/auth/**` matches an `auth` dir **anywhere** (`auth/x`, `db/auth/x`);
///   `**/` can match zero leading segments;
/// - `dir/**` (prefix — everything under `dir`);
/// - `*.ext` (extension);
/// - a bare directory or one with a trailing slash — `dir` and `dir/` both mean `dir/**` (this is why
///   a protected entry like `auth/` matches `auth/login.rs`);
/// - an exact path.
pub fn glob_match(glob: &str, path: &str) -> bool {
    // A leading `**/` matches any number of leading path segments, including none. Try the remainder
    // at the start and after every segment boundary.
    if let Some(rest) = glob.strip_prefix("**/") {
        if glob_match(rest, path) {
            return true;
        }
        let mut idx = 0;
        while let Some(slash) = path[idx..].find('/') {
            idx += slash + 1;
            if glob_match(rest, &path[idx..]) {
                return true;
            }
        }
        return false;
    }
    if let Some(dir) = glob.strip_suffix("/**") {
        return path == dir || path.starts_with(&format!("{dir}/"));
    }
    if let Some(ext) = glob.strip_prefix("*.") {
        return path.ends_with(&format!(".{ext}"));
    }
    // Normalize a trailing slash away (`dir/` ⇒ `dir`), then treat a bare directory as a prefix match
    // — `dir` covers `dir` itself and everything under `dir/`. An exact file path still matches via the
    // `path == dir` arm.
    let dir = glob.strip_suffix('/').unwrap_or(glob);
    path == dir || path.starts_with(&format!("{dir}/"))
}

/// A thread-safe in-memory [`Store`] for the scaffold and tests.
#[derive(Default)]
pub struct InMemory {
    actors: RwLock<HashMap<String, Actor>>,
    accounts: RwLock<HashMap<String, Account>>,
    repos: RwLock<HashMap<String, Repo>>,
    issues: RwLock<Vec<Issue>>,
    prs: RwLock<Vec<PullRequest>>,
    reviews: RwLock<Vec<Review>>,
    comments: RwLock<Vec<Comment>>,
    sessions: RwLock<Vec<SessionRecord>>,
    owners: RwLock<HashMap<String, Vec<OwnerRule>>>,
    projects: RwLock<Vec<Project>>,
    users: RwLock<HashMap<String, User>>,
    teams: RwLock<HashMap<String, Team>>,
    ai_conns: RwLock<Vec<AiConnection>>,
    ai_rotate: RwLock<HashMap<String, bool>>,
    ai_usage: RwLock<HashMap<String, crate::AiUsage>>,
}

impl InMemory {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl Store for InMemory {
    async fn put_actor(&self, actor: Actor) {
        self.actors.write().unwrap().insert(actor.id.clone(), actor);
    }
    async fn actor(&self, id: &str) -> Option<Actor> {
        self.actors.read().unwrap().get(id).cloned()
    }
    async fn actors(&self) -> Vec<Actor> {
        self.actors.read().unwrap().values().cloned().collect()
    }
    async fn put_account(&self, account: Account) {
        self.accounts.write().unwrap().insert(account.id.clone(), account);
    }
    async fn accounts(&self) -> Vec<Account> {
        self.accounts.read().unwrap().values().cloned().collect()
    }
    async fn put_repo(&self, repo: Repo) {
        self.repos.write().unwrap().insert(repo.id.clone(), repo);
    }
    async fn repos(&self) -> Vec<Repo> {
        self.repos.read().unwrap().values().cloned().collect()
    }
    async fn remove_repo(&self, id: &str) -> bool {
        self.repos.write().unwrap().remove(id).is_some()
    }
    async fn purge_repo_data(&self, repo_key: &str) {
        self.issues.write().unwrap().retain(|i| i.repo != repo_key);
        self.prs.write().unwrap().retain(|p| p.repo != repo_key);
        self.reviews.write().unwrap().retain(|r| r.repo != repo_key);
        self.comments.write().unwrap().retain(|c| c.repo != repo_key);
        self.sessions.write().unwrap().retain(|s| s.repo != repo_key);
        self.owners.write().unwrap().remove(repo_key);
    }
    async fn rekey_repo_data(&self, old_key: &str, new_key: &str) {
        for i in self.issues.write().unwrap().iter_mut().filter(|i| i.repo == old_key) {
            i.repo = new_key.to_string();
        }
        for p in self.prs.write().unwrap().iter_mut().filter(|p| p.repo == old_key) {
            p.repo = new_key.to_string();
        }
        for r in self.reviews.write().unwrap().iter_mut().filter(|r| r.repo == old_key) {
            r.repo = new_key.to_string();
        }
        for c in self.comments.write().unwrap().iter_mut().filter(|c| c.repo == old_key) {
            c.repo = new_key.to_string();
        }
        for s in self.sessions.write().unwrap().iter_mut().filter(|s| s.repo == old_key) {
            s.repo = new_key.to_string();
        }
        let mut owners = self.owners.write().unwrap();
        if let Some(rules) = owners.remove(old_key) {
            owners.insert(new_key.to_string(), rules);
        }
    }
    async fn put_issue(&self, issue: Issue) {
        self.issues.write().unwrap().push(issue);
    }
    async fn issues(&self, repo: &str) -> Vec<Issue> {
        self.issues.read().unwrap().iter().filter(|i| i.repo == repo).cloned().collect()
    }
    async fn replace_issue(&self, issue: Issue) -> bool {
        let mut g = self.issues.write().unwrap();
        match g.iter_mut().find(|i| i.repo == issue.repo && i.number == issue.number) {
            Some(slot) => {
                *slot = issue;
                true
            }
            None => false,
        }
    }
    async fn update_issue_content(&self, repo: &str, number: u64, title: Option<&str>, body: Option<&str>, edited_unix: u64) -> bool {
        let mut g = self.issues.write().unwrap();
        if let Some(i) = g.iter_mut().find(|i| i.repo == repo && i.number == number) {
            if let Some(t) = title {
                i.title = t.to_string();
            }
            if let Some(b) = body {
                i.body = b.to_string();
            }
            i.edited_unix = Some(edited_unix);
            true
        } else {
            false
        }
    }
    async fn put_pr(&self, pr: PullRequest) {
        self.prs.write().unwrap().push(pr);
    }
    async fn prs(&self, repo: &str) -> Vec<PullRequest> {
        self.prs.read().unwrap().iter().filter(|p| p.repo == repo).cloned().collect()
    }
    async fn replace_pr(&self, pr: PullRequest) -> bool {
        let mut g = self.prs.write().unwrap();
        match g.iter_mut().find(|p| p.repo == pr.repo && p.number == pr.number) {
            Some(slot) => {
                *slot = pr;
                true
            }
            None => false,
        }
    }
    async fn put_review(&self, review: Review) {
        self.reviews.write().unwrap().push(review);
    }
    async fn reviews(&self, repo: &str) -> Vec<Review> {
        self.reviews.read().unwrap().iter().filter(|r| r.repo == repo).cloned().collect()
    }
    async fn put_comment(&self, comment: Comment) {
        self.comments.write().unwrap().push(comment);
    }
    async fn comments(&self, repo: &str) -> Vec<Comment> {
        self.comments.read().unwrap().iter().filter(|c| c.repo == repo).cloned().collect()
    }
    async fn remove_comment(&self, repo: &str, id: &str) -> bool {
        let mut g = self.comments.write().unwrap();
        let before = g.len();
        g.retain(|c| !(c.repo == repo && c.id == id));
        g.len() != before
    }
    async fn update_comment_body(&self, repo: &str, id: &str, body: &str, edited_unix: u64) -> bool {
        let mut g = self.comments.write().unwrap();
        if let Some(c) = g.iter_mut().find(|c| c.repo == repo && c.id == id) {
            c.body = body.to_string();
            c.edited_unix = Some(edited_unix);
            true
        } else {
            false
        }
    }
    async fn put_session_record(&self, record: SessionRecord) {
        let mut g = self.sessions.write().unwrap();
        g.retain(|s| !(s.repo == record.repo && s.change == record.change));
        g.push(record);
    }
    async fn session_record(&self, repo: &str, change: &str) -> Option<SessionRecord> {
        self.sessions.read().unwrap().iter().find(|s| s.repo == repo && s.change == change).cloned()
    }
    async fn put_project(&self, project: Project) {
        self.projects.write().unwrap().push(project);
    }
    async fn projects(&self, owner: &str) -> Vec<Project> {
        self.projects.read().unwrap().iter().filter(|p| p.owner == owner).cloned().collect()
    }
    async fn put_ai_connection(&self, conn: AiConnection) {
        self.ai_conns.write().unwrap().push(conn);
    }
    async fn ai_connections(&self, owner: &str) -> Vec<AiConnection> {
        self.ai_conns.read().unwrap().iter().filter(|c| c.owner == owner).cloned().collect()
    }
    async fn remove_ai_connection(&self, owner: &str, id: &str) -> bool {
        let mut g = self.ai_conns.write().unwrap();
        let n = g.len();
        g.retain(|c| !(c.owner == owner && c.id == id));
        g.len() != n
    }
    async fn set_ai_rotate(&self, owner: &str, on: bool) {
        self.ai_rotate.write().unwrap().insert(owner.to_string(), on);
    }
    async fn ai_rotate(&self, owner: &str) -> bool {
        self.ai_rotate.read().unwrap().get(owner).copied().unwrap_or(false)
    }
    async fn add_ai_usage(&self, conn_id: &str, input: u64, output: u64, cost_micros: u64, at_unix: u64) {
        let mut g = self.ai_usage.write().unwrap();
        let u = g.entry(conn_id.to_string()).or_default();
        u.input_tokens += input;
        u.output_tokens += output;
        u.cost_micros += cost_micros;
        u.runs += 1;
        u.updated_unix = at_unix;
    }
    async fn ai_usage(&self, conn_id: &str) -> crate::AiUsage {
        self.ai_usage.read().unwrap().get(conn_id).cloned().unwrap_or_default()
    }
    async fn set_owners(&self, repo: &str, rules: Vec<OwnerRule>) {
        self.owners.write().unwrap().insert(repo.to_string(), rules);
    }
    async fn owners(&self, repo: &str) -> Vec<OwnerRule> {
        self.owners.read().unwrap().get(repo).cloned().unwrap_or_default()
    }
    async fn put_user(&self, user: User) {
        self.users.write().unwrap().insert(user.id.clone(), user);
    }
    async fn user(&self, id: &str) -> Option<User> {
        self.users.read().unwrap().get(id).cloned()
    }
    async fn user_by_username(&self, username: &str) -> Option<User> {
        self.users.read().unwrap().values().find(|u| u.username.eq_ignore_ascii_case(username)).cloned()
    }
    async fn user_by_actor(&self, actor: &str) -> Option<User> {
        self.users.read().unwrap().values().find(|u| u.actor == actor).cloned()
    }
    async fn users(&self) -> Vec<User> {
        self.users.read().unwrap().values().cloned().collect()
    }
    async fn put_team(&self, team: Team) {
        self.teams.write().unwrap().insert(team.id.clone(), team);
    }
    async fn team(&self, id: &str) -> Option<Team> {
        self.teams.read().unwrap().get(id).cloned()
    }
    async fn teams(&self, account: &str) -> Vec<Team> {
        self.teams.read().unwrap().values().filter(|t| t.account == account).cloned().collect()
    }
    async fn delete_team(&self, id: &str) {
        self.teams.write().unwrap().remove(id);
    }
    async fn ready(&self) -> bool {
        // Purely in-memory: no external dependency to check, always ready.
        true
    }
}

/// The full domain state, serialized as one JSON snapshot — the on-disk form of [`FileStore`].
#[derive(Default, Serialize, Deserialize)]
struct Snapshot {
    #[serde(default)]
    actors: HashMap<String, Actor>,
    #[serde(default)]
    accounts: HashMap<String, Account>,
    #[serde(default)]
    repos: HashMap<String, Repo>,
    #[serde(default)]
    issues: Vec<Issue>,
    #[serde(default)]
    prs: Vec<PullRequest>,
    #[serde(default)]
    reviews: Vec<Review>,
    #[serde(default)]
    comments: Vec<Comment>,
    #[serde(default)]
    sessions: Vec<SessionRecord>,
    #[serde(default)]
    owners: HashMap<String, Vec<OwnerRule>>,
    #[serde(default)]
    projects: Vec<Project>,
    #[serde(default)]
    users: HashMap<String, User>,
    #[serde(default)]
    teams: HashMap<String, Team>,
    #[serde(default)]
    ai_conns: Vec<AiConnection>,
    #[serde(default)]
    ai_rotate: HashMap<String, bool>,
    #[serde(default)]
    ai_usage: HashMap<String, crate::AiUsage>,
}

/// Load a [`Snapshot`] from disk, applying the corrupt-file guard: a **missing** file starts empty
/// (fresh install); a file that **exists but won't parse** is preserved (copied aside) and we panic
/// rather than silently start empty and let the next write overwrite recoverable data. Shared by
/// [`FileStore::open`] and the Postgres importer ([`import_store_json`]) so both honor the guard.
fn load_snapshot(path: &Path) -> Snapshot {
    match std::fs::read(path) {
        Err(_) => Snapshot::default(), // no file yet — a fresh store
        Ok(bytes) => match serde_json::from_slice::<Snapshot>(&bytes) {
            Ok(snap) => snap,
            Err(e) => {
                let backup = path.with_extension(format!("json.corrupt-{}", std::process::id()));
                let _ = std::fs::copy(path, &backup);
                panic!(
                    "hull: store at {} failed to parse ({e}); preserved a copy at {} and refused \
                     to start empty (which would overwrite your data). Migrate or restore, then restart.",
                    path.display(),
                    backup.display(),
                );
            }
        },
    }
}

/// A durable [`Store`] backed by a JSON snapshot on disk, so issues/accounts survive restarts.
/// Every mutation rewrites the snapshot atomically (temp file + rename). Fine for the current
/// scale; the SQL-backed store (M1+) replaces it when write volume grows. Content and provenance
/// still live in keel — this only persists Hull's relational domain objects.
pub struct FileStore {
    path: PathBuf,
    inner: RwLock<Snapshot>,
}

impl FileStore {
    /// Open the store at `path`. A **missing** file starts empty (fresh install). A file that
    /// **exists but won't parse** is NOT silently discarded — that would let the next write overwrite
    /// recoverable data (e.g. after an incompatible schema change). Instead we preserve a copy and
    /// refuse to start, so an operator can migrate rather than lose data.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let inner = load_snapshot(&path);
        FileStore { path, inner: RwLock::new(inner) }
    }

    /// Persist the current snapshot **durably and atomically**: write a temp file, `sync_all` its
    /// contents to disk, `rename` it over the target (an atomic replace), then best-effort fsync the
    /// parent directory so the rename itself survives a crash.
    ///
    /// A persist failure is NOT fatal here (the [`Store`] trait is infallible — it returns `()`), but
    /// it is no longer swallowed with a quiet `eprintln!`: the in-memory state has now silently
    /// diverged from disk, so we log at a loud ERROR level AND flip the process-global
    /// [`crate::PERSISTENCE_DEGRADED`] flag, which the server's `/health` handler surfaces as 503.
    /// Fully propagating the error to the caller requires a fallible `Store` trait (follow-up).
    fn save(&self, snap: &Snapshot) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let bytes = match serde_json::to_vec_pretty(snap) {
            Ok(b) => b,
            Err(e) => {
                crate::mark_persistence_degraded();
                eprintln!(
                    "hull: ERROR failed to serialize store snapshot: {e} — persistence is DEGRADED \
                     (in-memory state has diverged from disk)."
                );
                return;
            }
        };
        let tmp = self.path.with_extension("json.tmp");
        let write_durably = || -> std::io::Result<()> {
            use std::io::Write;
            let f = std::fs::File::create(&tmp)?;
            {
                let mut w = std::io::BufWriter::new(&f);
                w.write_all(&bytes)?;
                w.flush()?;
            }
            f.sync_all()?; // temp file contents are on disk before the rename makes them live
            std::fs::rename(&tmp, &self.path)?;
            Ok(())
        };
        if let Err(e) = write_durably() {
            crate::mark_persistence_degraded();
            eprintln!(
                "hull: ERROR failed to persist store to {}: {e} — persistence is DEGRADED (in-memory \
                 state has diverged from disk; a restart would lose un-persisted writes).",
                self.path.display()
            );
            return;
        }
        // Best-effort: fsync the directory so the rename is durable across a crash. Opening a dir as a
        // File and syncing it is a no-op / unsupported on some platforms (e.g. Windows) — a failure
        // here alone is not treated as data loss.
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
        }
        // The snapshot is now fully on disk: whatever transient failure may have set the degraded flag
        // is resolved (a full snapshot rewrite re-syncs disk to memory), so clear it and let `/health`
        // recover to 200.
        crate::clear_persistence_degraded();
    }

    fn mutate(&self, f: impl FnOnce(&mut Snapshot)) {
        let mut g = self.inner.write().unwrap();
        f(&mut g);
        self.save(&g);
    }
}

#[async_trait::async_trait]
impl Store for FileStore {
    async fn put_actor(&self, actor: Actor) {
        self.mutate(|s| {
            s.actors.insert(actor.id.clone(), actor);
        });
    }
    async fn actor(&self, id: &str) -> Option<Actor> {
        self.inner.read().unwrap().actors.get(id).cloned()
    }
    async fn actors(&self) -> Vec<Actor> {
        self.inner.read().unwrap().actors.values().cloned().collect()
    }
    async fn put_account(&self, account: Account) {
        self.mutate(|s| {
            s.accounts.insert(account.id.clone(), account);
        });
    }
    async fn accounts(&self) -> Vec<Account> {
        self.inner.read().unwrap().accounts.values().cloned().collect()
    }
    async fn put_repo(&self, repo: Repo) {
        self.mutate(|s| {
            s.repos.insert(repo.id.clone(), repo);
        });
    }
    async fn repos(&self) -> Vec<Repo> {
        self.inner.read().unwrap().repos.values().cloned().collect()
    }
    async fn remove_repo(&self, id: &str) -> bool {
        let mut removed = false;
        self.mutate(|s| removed = s.repos.remove(id).is_some());
        removed
    }
    async fn purge_repo_data(&self, repo_key: &str) {
        self.mutate(|s| {
            s.issues.retain(|i| i.repo != repo_key);
            s.prs.retain(|p| p.repo != repo_key);
            s.reviews.retain(|r| r.repo != repo_key);
            s.comments.retain(|c| c.repo != repo_key);
            s.sessions.retain(|x| x.repo != repo_key);
            s.owners.remove(repo_key);
        });
    }
    async fn rekey_repo_data(&self, old_key: &str, new_key: &str) {
        self.mutate(|s| {
            for i in s.issues.iter_mut().filter(|i| i.repo == old_key) {
                i.repo = new_key.to_string();
            }
            for p in s.prs.iter_mut().filter(|p| p.repo == old_key) {
                p.repo = new_key.to_string();
            }
            for r in s.reviews.iter_mut().filter(|r| r.repo == old_key) {
                r.repo = new_key.to_string();
            }
            for c in s.comments.iter_mut().filter(|c| c.repo == old_key) {
                c.repo = new_key.to_string();
            }
            for x in s.sessions.iter_mut().filter(|x| x.repo == old_key) {
                x.repo = new_key.to_string();
            }
            if let Some(rules) = s.owners.remove(old_key) {
                s.owners.insert(new_key.to_string(), rules);
            }
        });
    }
    async fn put_issue(&self, issue: Issue) {
        self.mutate(|s| s.issues.push(issue));
    }
    async fn issues(&self, repo: &str) -> Vec<Issue> {
        self.inner.read().unwrap().issues.iter().filter(|i| i.repo == repo).cloned().collect()
    }
    async fn replace_issue(&self, issue: Issue) -> bool {
        let mut g = self.inner.write().unwrap();
        let replaced = match g.issues.iter_mut().find(|i| i.repo == issue.repo && i.number == issue.number) {
            Some(slot) => {
                *slot = issue;
                true
            }
            None => false,
        };
        if replaced {
            self.save(&g);
        }
        replaced
    }
    async fn update_issue_content(&self, repo: &str, number: u64, title: Option<&str>, body: Option<&str>, edited_unix: u64) -> bool {
        let mut updated = false;
        self.mutate(|s| {
            if let Some(i) = s.issues.iter_mut().find(|i| i.repo == repo && i.number == number) {
                if let Some(t) = title {
                    i.title = t.to_string();
                }
                if let Some(b) = body {
                    i.body = b.to_string();
                }
                i.edited_unix = Some(edited_unix);
                updated = true;
            }
        });
        updated
    }
    async fn put_pr(&self, pr: PullRequest) {
        self.mutate(|s| s.prs.push(pr));
    }
    async fn prs(&self, repo: &str) -> Vec<PullRequest> {
        self.inner.read().unwrap().prs.iter().filter(|p| p.repo == repo).cloned().collect()
    }
    async fn replace_pr(&self, pr: PullRequest) -> bool {
        let mut g = self.inner.write().unwrap();
        let replaced = match g.prs.iter_mut().find(|p| p.repo == pr.repo && p.number == pr.number) {
            Some(slot) => {
                *slot = pr;
                true
            }
            None => false,
        };
        if replaced {
            self.save(&g);
        }
        replaced
    }
    async fn put_review(&self, review: Review) {
        self.mutate(|s| s.reviews.push(review));
    }
    async fn reviews(&self, repo: &str) -> Vec<Review> {
        self.inner.read().unwrap().reviews.iter().filter(|r| r.repo == repo).cloned().collect()
    }
    async fn put_comment(&self, comment: Comment) {
        self.mutate(|s| s.comments.push(comment));
    }
    async fn comments(&self, repo: &str) -> Vec<Comment> {
        self.inner.read().unwrap().comments.iter().filter(|c| c.repo == repo).cloned().collect()
    }
    async fn remove_comment(&self, repo: &str, id: &str) -> bool {
        let mut removed = false;
        self.mutate(|s| { let before = s.comments.len(); s.comments.retain(|c| !(c.repo == repo && c.id == id)); removed = s.comments.len() != before; });
        removed
    }
    async fn update_comment_body(&self, repo: &str, id: &str, body: &str, edited_unix: u64) -> bool {
        let mut updated = false;
        self.mutate(|s| {
            if let Some(c) = s.comments.iter_mut().find(|c| c.repo == repo && c.id == id) {
                c.body = body.to_string();
                c.edited_unix = Some(edited_unix);
                updated = true;
            }
        });
        updated
    }
    async fn put_session_record(&self, record: SessionRecord) {
        self.mutate(|s| {
            s.sessions.retain(|x| !(x.repo == record.repo && x.change == record.change));
            s.sessions.push(record);
        });
    }
    async fn session_record(&self, repo: &str, change: &str) -> Option<SessionRecord> {
        self.inner.read().unwrap().sessions.iter().find(|s| s.repo == repo && s.change == change).cloned()
    }
    async fn put_project(&self, project: Project) {
        self.mutate(|s| s.projects.push(project));
    }
    async fn projects(&self, owner: &str) -> Vec<Project> {
        self.inner.read().unwrap().projects.iter().filter(|p| p.owner == owner).cloned().collect()
    }
    async fn put_ai_connection(&self, conn: AiConnection) {
        self.mutate(|s| s.ai_conns.push(conn));
    }
    async fn ai_connections(&self, owner: &str) -> Vec<AiConnection> {
        self.inner.read().unwrap().ai_conns.iter().filter(|c| c.owner == owner).cloned().collect()
    }
    async fn remove_ai_connection(&self, owner: &str, id: &str) -> bool {
        let mut removed = false;
        self.mutate(|s| { let n = s.ai_conns.len(); s.ai_conns.retain(|c| !(c.owner == owner && c.id == id)); removed = s.ai_conns.len() != n; });
        removed
    }
    async fn set_ai_rotate(&self, owner: &str, on: bool) {
        self.mutate(|s| { s.ai_rotate.insert(owner.to_string(), on); });
    }
    async fn ai_rotate(&self, owner: &str) -> bool {
        self.inner.read().unwrap().ai_rotate.get(owner).copied().unwrap_or(false)
    }
    async fn add_ai_usage(&self, conn_id: &str, input: u64, output: u64, cost_micros: u64, at_unix: u64) {
        self.mutate(|s| {
            let u = s.ai_usage.entry(conn_id.to_string()).or_default();
            u.input_tokens += input;
            u.output_tokens += output;
            u.cost_micros += cost_micros;
            u.runs += 1;
            u.updated_unix = at_unix;
        });
    }
    async fn ai_usage(&self, conn_id: &str) -> crate::AiUsage {
        self.inner.read().unwrap().ai_usage.get(conn_id).cloned().unwrap_or_default()
    }
    async fn set_owners(&self, repo: &str, rules: Vec<OwnerRule>) {
        self.mutate(|s| {
            s.owners.insert(repo.to_string(), rules);
        });
    }
    async fn owners(&self, repo: &str) -> Vec<OwnerRule> {
        self.inner.read().unwrap().owners.get(repo).cloned().unwrap_or_default()
    }
    async fn put_user(&self, user: User) {
        self.mutate(|s| {
            s.users.insert(user.id.clone(), user);
        });
    }
    async fn user(&self, id: &str) -> Option<User> {
        self.inner.read().unwrap().users.get(id).cloned()
    }
    async fn user_by_username(&self, username: &str) -> Option<User> {
        self.inner.read().unwrap().users.values().find(|u| u.username.eq_ignore_ascii_case(username)).cloned()
    }
    async fn user_by_actor(&self, actor: &str) -> Option<User> {
        self.inner.read().unwrap().users.values().find(|u| u.actor == actor).cloned()
    }
    async fn users(&self) -> Vec<User> {
        self.inner.read().unwrap().users.values().cloned().collect()
    }
    async fn put_team(&self, team: Team) {
        self.mutate(|s| {
            s.teams.insert(team.id.clone(), team);
        });
    }
    async fn team(&self, id: &str) -> Option<Team> {
        self.inner.read().unwrap().teams.get(id).cloned()
    }
    async fn teams(&self, account: &str) -> Vec<Team> {
        self.inner.read().unwrap().teams.values().filter(|t| t.account == account).cloned().collect()
    }
    async fn delete_team(&self, id: &str) {
        self.mutate(|s| {
            s.teams.remove(id);
        });
    }
    async fn ready(&self) -> bool {
        // Ready as long as the data directory is reachable (a cheap check that the durable path is
        // still writable-ish). A missing parent dir means writes would fail, so report not-ready.
        // The snapshot file itself may legitimately not exist yet on a fresh install, so we check the
        // parent directory rather than the file.
        match self.path.parent() {
            Some(dir) => dir.is_dir(),
            // No parent (e.g. a bare relative filename) → cwd, which exists.
            None => true,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// Postgres-backed [`Store`] (M1).
//
// The trait is ASYNC; concurrency comes from a `deadpool-postgres` pool over the async
// `tokio-postgres` driver. Every method checks out a pooled connection and `.await`s its SQL, so it
// runs natively on the server's tokio runtime (a blocking client would nest its own runtime and panic
// under `#[tokio::main]`). Each domain object is stored as one row: its full serialized form in a
// JSONB `data` column, with the fields the trait matches/filters on lifted into indexed key columns.
// Reads deserialize `data` back into the exact domain type, so this is a faithful mirror of
// `Snapshot`/`InMemory`.
//
// Error policy: a SQL/connection failure inside a trait method is exceptional (the DB being down is
// not a normal condition) and panics via `.expect(...)`, consistent with `InMemory` unwrapping its
// locks. `connect` returns `Result` so wiring can surface a bad URL / failed migration at startup.
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// Embedded, ordered schema migrations. Index `i` is version `i + 1`; applied once, tracked in
/// `_hull_schema_version`. Append new files here (never edit an applied one) to evolve the schema.
const MIGRATIONS: &[&str] = &[include_str!("migrations/0001_init.sql")];

/// A durable [`Store`] backed by Postgres. Chosen at runtime when `HULL_DATABASE_URL` is set; the
/// default (unset) path keeps [`FileStore`]. Content/provenance still live in keel — this persists
/// only Hull's relational domain objects, referencing keel objects by content address.
pub struct PostgresStore {
    pool: Pool,
}

impl PostgresStore {
    /// Connect (building the pool), then run migrations. `database_url` is a standard libpq URL,
    /// e.g. `postgres://user:pass@host:5432/hull`. Returns an error string on a bad URL, an
    /// unreachable server, or a failed migration — the caller decides whether that's fatal. The
    /// deadpool pool is built lazily (connections are opened on first checkout), so a bad server is
    /// surfaced by the migration run's first `pool.get().await`, not by pool construction.
    pub async fn connect(database_url: &str) -> Result<Self, String> {
        // Parse the libpq URL through tokio-postgres, then hand its fields to deadpool's config so the
        // whole standard URL form keeps working.
        let pg_config = database_url
            .parse::<tokio_postgres::Config>()
            .map_err(|e| format!("hull: invalid HULL_DATABASE_URL: {e}"))?;
        let mut cfg = PgConfig::new();
        cfg.manager = Some(ManagerConfig { recycling_method: RecyclingMethod::Fast });
        copy_pg_config(&pg_config, &mut cfg);
        let pool = cfg
            .create_pool(Some(Runtime::Tokio1), NoTls)
            .map_err(|e| format!("hull: failed to build Postgres pool: {e}"))?;
        let store = PostgresStore { pool };
        store.migrate().await.map_err(|e| format!("hull: Postgres migration failed: {e}"))?;
        Ok(store)
    }

    /// Check out a pooled connection. Panics if the pool can't hand one out (server gone) — see the
    /// module error policy.
    async fn conn(&self) -> deadpool_postgres::Client {
        self.pool.get().await.expect("hull: postgres connection pool exhausted or server unreachable")
    }

    /// Apply any migrations not yet recorded in `_hull_schema_version`, each in its own transaction.
    async fn migrate(&self) -> Result<(), tokio_postgres::Error> {
        let mut c = self.conn().await;
        c.batch_execute(
            "CREATE TABLE IF NOT EXISTS _hull_schema_version (version INT PRIMARY KEY, applied_unix BIGINT NOT NULL)",
        )
        .await?;
        let current: i32 = c.query_one("SELECT COALESCE(MAX(version), 0) FROM _hull_schema_version", &[]).await?.get(0);
        let now = now_unix();
        for (idx, sql) in MIGRATIONS.iter().enumerate() {
            let version = idx as i32 + 1;
            if version > current {
                let tx = c.transaction().await?;
                tx.batch_execute(sql).await?;
                // Idempotent bookkeeping insert: on a fresh DB two instances can both read current=0
                // and race to apply v1. The migration DDL is re-runnable (IF NOT EXISTS) and this
                // ON CONFLICT DO NOTHING keeps the loser from failing on the version PK — whichever
                // commits first wins, the other's transaction is a harmless no-op. Still run-once per
                // migration in the steady state (the `version > current` guard skips applied ones).
                tx.execute(
                    "INSERT INTO _hull_schema_version (version, applied_unix) VALUES ($1, $2) ON CONFLICT (version) DO NOTHING",
                    &[&version, &now],
                )
                .await?;
                tx.commit().await?;
            }
        }
        Ok(())
    }
}

/// Copy the connection fields deadpool needs out of a parsed [`tokio_postgres::Config`] (which is how
/// we accept the standard libpq URL form) into a [`deadpool_postgres::Config`]. deadpool builds its
/// own `tokio_postgres::Config` internally, so we bridge the parsed URL's host/port/user/etc. across.
fn copy_pg_config(src: &tokio_postgres::Config, dst: &mut PgConfig) {
    if let Some(host) = src.get_hosts().iter().map(|h| match h {
        tokio_postgres::config::Host::Tcp(s) => s.clone(),
        #[cfg(unix)]
        tokio_postgres::config::Host::Unix(p) => p.to_string_lossy().into_owned(),
    }).next() {
        dst.host = Some(host);
    }
    if let Some(port) = src.get_ports().first() {
        dst.port = Some(*port);
    }
    if let Some(user) = src.get_user() {
        dst.user = Some(user.to_string());
    }
    if let Some(pass) = src.get_password() {
        dst.password = Some(String::from_utf8_lossy(pass).into_owned());
    }
    if let Some(db) = src.get_dbname() {
        dst.dbname = Some(db.to_string());
    }
}

/// Serialize a domain object to a JSONB value (panics only on a non-serializable type, which our
/// domain types never are).
fn to_json<T: Serialize>(v: &T) -> serde_json::Value {
    serde_json::to_value(v).expect("hull: domain object failed to serialize")
}

/// Deserialize a JSONB `data` column back into its domain type.
fn from_json<T: DeserializeOwned>(v: serde_json::Value) -> T {
    serde_json::from_value(v).expect("hull: stored row failed to deserialize into its domain type")
}

fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[async_trait::async_trait]
impl Store for PostgresStore {
    async fn put_actor(&self, actor: Actor) {
        self.conn().await
            .execute(
                "INSERT INTO actors (id, data) VALUES ($1, $2) ON CONFLICT (id) DO UPDATE SET data = EXCLUDED.data",
                &[&actor.id, &to_json(&actor)],
            )
            .await.expect("put_actor");
    }
    async fn actor(&self, id: &str) -> Option<Actor> {
        self.conn().await.query_opt("SELECT data FROM actors WHERE id = $1", &[&id]).await.expect("actor").map(|r| from_json(r.get(0)))
    }
    async fn actors(&self) -> Vec<Actor> {
        self.conn().await.query("SELECT data FROM actors", &[]).await.expect("actors").into_iter().map(|r| from_json(r.get(0))).collect()
    }
    async fn put_account(&self, account: Account) {
        self.conn().await
            .execute(
                "INSERT INTO accounts (id, data) VALUES ($1, $2) ON CONFLICT (id) DO UPDATE SET data = EXCLUDED.data",
                &[&account.id, &to_json(&account)],
            )
            .await.expect("put_account");
    }
    async fn accounts(&self) -> Vec<Account> {
        self.conn().await.query("SELECT data FROM accounts", &[]).await.expect("accounts").into_iter().map(|r| from_json(r.get(0))).collect()
    }
    async fn put_repo(&self, repo: Repo) {
        self.conn().await
            .execute(
                "INSERT INTO repos (id, owner, name, data) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (id) DO UPDATE SET owner = EXCLUDED.owner, name = EXCLUDED.name, data = EXCLUDED.data",
                &[&repo.id, &repo.owner, &repo.name, &to_json(&repo)],
            )
            .await.expect("put_repo");
    }
    async fn repos(&self) -> Vec<Repo> {
        self.conn().await.query("SELECT data FROM repos", &[]).await.expect("repos").into_iter().map(|r| from_json(r.get(0))).collect()
    }
    async fn remove_repo(&self, id: &str) -> bool {
        self.conn().await.execute("DELETE FROM repos WHERE id = $1", &[&id]).await.expect("remove_repo") > 0
    }
    async fn purge_repo_data(&self, repo_key: &str) {
        let mut c = self.conn().await;
        let tx = c.transaction().await.expect("purge tx");
        for table in ["issues", "prs", "reviews", "comments", "session_records", "owners"] {
            tx.execute(&format!("DELETE FROM {table} WHERE repo = $1"), &[&repo_key]).await.expect("purge delete");
        }
        tx.commit().await.expect("purge commit");
    }
    async fn rekey_repo_data(&self, old_key: &str, new_key: &str) {
        let mut c = self.conn().await;
        let tx = c.transaction().await.expect("rekey tx");
        // These carry `repo` both as a key column AND inside `data` — move both so the stored object
        // matches the in-memory rekey (which rewrites the struct's `repo` field).
        for table in ["issues", "prs", "reviews", "comments", "session_records"] {
            tx.execute(
                &format!("UPDATE {table} SET repo = $1, data = jsonb_set(data, '{{repo}}', to_jsonb($1::text)) WHERE repo = $2"),
                &[&new_key, &old_key],
            )
            .await.expect("rekey update");
        }
        // Owners are keyed only by repo (no `repo` field inside). Mirror the in-memory move: only if
        // the old key exists, and overwrite any rules already at the new key.
        if let Some(row) = tx.query_opt("SELECT rules FROM owners WHERE repo = $1", &[&old_key]).await.expect("rekey owners read") {
            let rules: serde_json::Value = row.get(0);
            tx.execute("DELETE FROM owners WHERE repo = $1 OR repo = $2", &[&old_key, &new_key]).await.expect("rekey owners clear");
            tx.execute("INSERT INTO owners (repo, rules) VALUES ($1, $2)", &[&new_key, &rules]).await.expect("rekey owners set");
        }
        tx.commit().await.expect("rekey commit");
    }
    async fn put_issue(&self, issue: Issue) {
        self.conn().await
            .execute("INSERT INTO issues (repo, number, data) VALUES ($1, $2, $3)", &[&issue.repo, &(issue.number as i64), &to_json(&issue)])
            .await.expect("put_issue");
    }
    async fn issues(&self, repo: &str) -> Vec<Issue> {
        self.conn().await.query("SELECT data FROM issues WHERE repo = $1", &[&repo]).await.expect("issues").into_iter().map(|r| from_json(r.get(0))).collect()
    }
    async fn replace_issue(&self, issue: Issue) -> bool {
        // UPDATE … WHERE (repo, number) RETURNING — no read-your-write-in-memory assumption. The
        // match key is unchanged, so the `repo`/`number` columns stay consistent with `data`.
        self.conn().await
            .execute(
                "UPDATE issues SET data = $1 WHERE repo = $2 AND number = $3",
                &[&to_json(&issue), &issue.repo, &(issue.number as i64)],
            )
            .await.expect("replace_issue")
            > 0
    }
    async fn update_issue_content(&self, repo: &str, number: u64, title: Option<&str>, body: Option<&str>, edited_unix: u64) -> bool {
        // Atomic top-level JSONB merge (`data || patch`): stamp edited_unix, and set title/body only
        // when provided. No round-trip; `None` leaves the field untouched.
        let mut patch = serde_json::Map::new();
        patch.insert("edited_unix".into(), serde_json::json!(edited_unix));
        if let Some(t) = title {
            patch.insert("title".into(), serde_json::json!(t));
        }
        if let Some(b) = body {
            patch.insert("body".into(), serde_json::json!(b));
        }
        self.conn().await
            .execute(
                "UPDATE issues SET data = data || $1 WHERE repo = $2 AND number = $3",
                &[&serde_json::Value::Object(patch), &repo, &(number as i64)],
            )
            .await.expect("update_issue_content")
            > 0
    }
    async fn put_pr(&self, pr: PullRequest) {
        self.conn().await
            .execute("INSERT INTO prs (repo, number, data) VALUES ($1, $2, $3)", &[&pr.repo, &(pr.number as i64), &to_json(&pr)])
            .await.expect("put_pr");
    }
    async fn prs(&self, repo: &str) -> Vec<PullRequest> {
        self.conn().await.query("SELECT data FROM prs WHERE repo = $1", &[&repo]).await.expect("prs").into_iter().map(|r| from_json(r.get(0))).collect()
    }
    async fn replace_pr(&self, pr: PullRequest) -> bool {
        self.conn().await
            .execute("UPDATE prs SET data = $1 WHERE repo = $2 AND number = $3", &[&to_json(&pr), &pr.repo, &(pr.number as i64)])
            .await.expect("replace_pr")
            > 0
    }
    async fn put_review(&self, review: Review) {
        self.conn().await
            .execute("INSERT INTO reviews (id, repo, data) VALUES ($1, $2, $3)", &[&review.id, &review.repo, &to_json(&review)])
            .await.expect("put_review");
    }
    async fn reviews(&self, repo: &str) -> Vec<Review> {
        self.conn().await.query("SELECT data FROM reviews WHERE repo = $1", &[&repo]).await.expect("reviews").into_iter().map(|r| from_json(r.get(0))).collect()
    }
    async fn put_comment(&self, comment: Comment) {
        self.conn().await
            .execute("INSERT INTO comments (id, repo, data) VALUES ($1, $2, $3)", &[&comment.id, &comment.repo, &to_json(&comment)])
            .await.expect("put_comment");
    }
    async fn comments(&self, repo: &str) -> Vec<Comment> {
        self.conn().await.query("SELECT data FROM comments WHERE repo = $1", &[&repo]).await.expect("comments").into_iter().map(|r| from_json(r.get(0))).collect()
    }
    async fn remove_comment(&self, repo: &str, id: &str) -> bool {
        self.conn().await.execute("DELETE FROM comments WHERE repo = $1 AND id = $2", &[&repo, &id]).await.expect("remove_comment") > 0
    }
    async fn update_comment_body(&self, repo: &str, id: &str, body: &str, edited_unix: u64) -> bool {
        let patch = serde_json::json!({ "body": body, "edited_unix": edited_unix });
        self.conn().await
            .execute("UPDATE comments SET data = data || $1 WHERE repo = $2 AND id = $3", &[&patch, &repo, &id])
            .await.expect("update_comment_body")
            > 0
    }
    async fn put_session_record(&self, record: SessionRecord) {
        // Latest write wins per (repo, change) — upsert on the unique key.
        self.conn().await
            .execute(
                "INSERT INTO session_records (repo, change, data) VALUES ($1, $2, $3) \
                 ON CONFLICT (repo, change) DO UPDATE SET data = EXCLUDED.data",
                &[&record.repo, &record.change, &to_json(&record)],
            )
            .await.expect("put_session_record");
    }
    async fn session_record(&self, repo: &str, change: &str) -> Option<SessionRecord> {
        self.conn().await
            .query_opt("SELECT data FROM session_records WHERE repo = $1 AND change = $2", &[&repo, &change])
            .await.expect("session_record")
            .map(|r| from_json(r.get(0)))
    }
    async fn put_project(&self, project: Project) {
        self.conn().await
            .execute("INSERT INTO projects (id, owner, data) VALUES ($1, $2, $3)", &[&project.id, &project.owner, &to_json(&project)])
            .await.expect("put_project");
    }
    async fn projects(&self, owner: &str) -> Vec<Project> {
        self.conn().await.query("SELECT data FROM projects WHERE owner = $1", &[&owner]).await.expect("projects").into_iter().map(|r| from_json(r.get(0))).collect()
    }
    async fn put_ai_connection(&self, conn: AiConnection) {
        self.conn().await
            .execute("INSERT INTO ai_connections (id, owner, data) VALUES ($1, $2, $3)", &[&conn.id, &conn.owner, &to_json(&conn)])
            .await.expect("put_ai_connection");
    }
    async fn ai_connections(&self, owner: &str) -> Vec<AiConnection> {
        self.conn().await.query("SELECT data FROM ai_connections WHERE owner = $1", &[&owner]).await.expect("ai_connections").into_iter().map(|r| from_json(r.get(0))).collect()
    }
    async fn remove_ai_connection(&self, owner: &str, id: &str) -> bool {
        self.conn().await.execute("DELETE FROM ai_connections WHERE owner = $1 AND id = $2", &[&owner, &id]).await.expect("remove_ai_connection") > 0
    }
    async fn set_ai_rotate(&self, owner: &str, on: bool) {
        self.conn().await
            .execute(
                "INSERT INTO ai_rotate (owner, on_flag) VALUES ($1, $2) ON CONFLICT (owner) DO UPDATE SET on_flag = EXCLUDED.on_flag",
                &[&owner, &on],
            )
            .await.expect("set_ai_rotate");
    }
    async fn ai_rotate(&self, owner: &str) -> bool {
        self.conn().await
            .query_opt("SELECT on_flag FROM ai_rotate WHERE owner = $1", &[&owner])
            .await.expect("ai_rotate")
            .map(|r| r.get(0))
            .unwrap_or(false)
    }
    async fn add_ai_usage(&self, conn_id: &str, input: u64, output: u64, cost_micros: u64, at_unix: u64) {
        // Atomic accumulate (input = input + delta, runs = runs + 1) — never read-modify-write.
        self.conn().await
            .execute(
                "INSERT INTO ai_usage (conn_id, input_tokens, output_tokens, cost_micros, runs, updated_unix) \
                 VALUES ($1, $2, $3, $4, 1, $5) \
                 ON CONFLICT (conn_id) DO UPDATE SET \
                   input_tokens  = ai_usage.input_tokens  + EXCLUDED.input_tokens, \
                   output_tokens = ai_usage.output_tokens + EXCLUDED.output_tokens, \
                   cost_micros   = ai_usage.cost_micros   + EXCLUDED.cost_micros, \
                   runs          = ai_usage.runs          + 1, \
                   updated_unix  = EXCLUDED.updated_unix",
                &[&conn_id, &(input as i64), &(output as i64), &(cost_micros as i64), &(at_unix as i64)],
            )
            .await.expect("add_ai_usage");
    }
    async fn ai_usage(&self, conn_id: &str) -> crate::AiUsage {
        self.conn().await
            .query_opt(
                "SELECT input_tokens, output_tokens, cost_micros, runs, updated_unix FROM ai_usage WHERE conn_id = $1",
                &[&conn_id],
            )
            .await.expect("ai_usage")
            .map(|r| crate::AiUsage {
                input_tokens: r.get::<_, i64>(0) as u64,
                output_tokens: r.get::<_, i64>(1) as u64,
                cost_micros: r.get::<_, i64>(2) as u64,
                runs: r.get::<_, i64>(3) as u64,
                updated_unix: r.get::<_, i64>(4) as u64,
            })
            .unwrap_or_default()
    }
    async fn set_owners(&self, repo: &str, rules: Vec<OwnerRule>) {
        self.conn().await
            .execute(
                "INSERT INTO owners (repo, rules) VALUES ($1, $2) ON CONFLICT (repo) DO UPDATE SET rules = EXCLUDED.rules",
                &[&repo, &to_json(&rules)],
            )
            .await.expect("set_owners");
    }
    async fn owners(&self, repo: &str) -> Vec<OwnerRule> {
        self.conn().await
            .query_opt("SELECT rules FROM owners WHERE repo = $1", &[&repo])
            .await.expect("owners")
            .map(|r| from_json(r.get(0)))
            .unwrap_or_default()
    }
    async fn put_user(&self, user: User) {
        self.conn().await
            .execute(
                "INSERT INTO users (id, username, actor, data) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (id) DO UPDATE SET username = EXCLUDED.username, actor = EXCLUDED.actor, data = EXCLUDED.data",
                &[&user.id, &user.username, &user.actor, &to_json(&user)],
            )
            .await.expect("put_user");
    }
    async fn user(&self, id: &str) -> Option<User> {
        self.conn().await.query_opt("SELECT data FROM users WHERE id = $1", &[&id]).await.expect("user").map(|r| from_json(r.get(0)))
    }
    async fn user_by_username(&self, username: &str) -> Option<User> {
        self.conn().await
            .query_opt("SELECT data FROM users WHERE lower(username) = lower($1)", &[&username])
            .await.expect("user_by_username")
            .map(|r| from_json(r.get(0)))
    }
    async fn user_by_actor(&self, actor: &str) -> Option<User> {
        self.conn().await
            .query_opt("SELECT data FROM users WHERE actor = $1 LIMIT 1", &[&actor])
            .await.expect("user_by_actor")
            .map(|r| from_json(r.get(0)))
    }
    async fn users(&self) -> Vec<User> {
        self.conn().await.query("SELECT data FROM users", &[]).await.expect("users").into_iter().map(|r| from_json(r.get(0))).collect()
    }
    async fn put_team(&self, team: Team) {
        self.conn().await
            .execute(
                "INSERT INTO teams (id, account, data) VALUES ($1, $2, $3) \
                 ON CONFLICT (id) DO UPDATE SET account = EXCLUDED.account, data = EXCLUDED.data",
                &[&team.id, &team.account, &to_json(&team)],
            )
            .await.expect("put_team");
    }
    async fn team(&self, id: &str) -> Option<Team> {
        self.conn().await.query_opt("SELECT data FROM teams WHERE id = $1", &[&id]).await.expect("team").map(|r| from_json(r.get(0)))
    }
    async fn teams(&self, account: &str) -> Vec<Team> {
        self.conn().await.query("SELECT data FROM teams WHERE account = $1", &[&account]).await.expect("teams").into_iter().map(|r| from_json(r.get(0))).collect()
    }
    async fn delete_team(&self, id: &str) {
        self.conn().await.execute("DELETE FROM teams WHERE id = $1", &[&id]).await.expect("delete_team");
    }
    async fn ready(&self) -> bool {
        // Unlike the other methods, readiness must NOT panic on a dead pool/server — that is exactly
        // the condition it exists to report. Check out a connection (bounded) and run a trivial query;
        // any failure at either step ⇒ not ready.
        match tokio::time::timeout(std::time::Duration::from_secs(2), self.pool.get()).await {
            Ok(Ok(client)) => client.simple_query("SELECT 1").await.is_ok(),
            _ => false,
        }
    }
}

/// How many rows a [`import_store_json`] run wrote, per entity. Purely informational (for the CLI).
#[derive(Debug, Default, Clone)]
pub struct ImportStats {
    pub actors: usize,
    pub accounts: usize,
    pub repos: usize,
    pub issues: usize,
    pub prs: usize,
    pub reviews: usize,
    pub comments: usize,
    pub sessions: usize,
    pub projects: usize,
    pub users: usize,
    pub teams: usize,
    pub ai_connections: usize,
    pub ai_rotate: usize,
    pub owners: usize,
    pub ai_usage: usize,
}

impl std::fmt::Display for ImportStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} actors, {} accounts, {} repos, {} issues, {} prs, {} reviews, {} comments, \
             {} sessions, {} projects, {} users, {} teams, {} ai_connections, {} ai_rotate, \
             {} owners, {} ai_usage",
            self.actors,
            self.accounts,
            self.repos,
            self.issues,
            self.prs,
            self.reviews,
            self.comments,
            self.sessions,
            self.projects,
            self.users,
            self.teams,
            self.ai_connections,
            self.ai_rotate,
            self.owners,
            self.ai_usage,
        )
    }
}

/// One-shot importer: read a `store.json` snapshot (honoring the same corrupt-file guard as
/// [`FileStore::open`]) and load every domain row into Postgres. **Idempotent**: the domain tables
/// are truncated first, so the resulting DB state is exactly the snapshot regardless of prior
/// contents. A missing file imports an empty snapshot (no-op). Does NOT touch the keel object store.
pub async fn import_store_json(pg: &PostgresStore, path: &Path) -> Result<ImportStats, String> {
    let snap = load_snapshot(path);
    let mut c = pg.conn().await;

    // The ENTIRE import — the TRUNCATE of every domain table AND every row insert — runs inside ONE
    // transaction on a SINGLE connection, committed only at the very end. Any failure (a bad row, a
    // unique violation from a snapshot that still holds case-variant-duplicate repos, a dropped
    // connection) rolls the whole thing back, leaving the pre-import data intact — never a truncated,
    // half-loaded DB. Inserts go straight to the transaction client with fallible SQL (no `.expect`)
    // rather than through the infallible `Store::put_*` methods, which each grab their own pooled
    // connection and panic on error. Re-running on a clean snapshot is idempotent (truncate-then-load).
    let tx = c.transaction().await.map_err(|e| format!("import: begin tx: {e}"))?;
    tx.batch_execute(
        "TRUNCATE actors, accounts, repos, issues, prs, reviews, comments, session_records, \
         projects, users, teams, ai_connections, ai_rotate, ai_usage, owners",
    )
    .await.map_err(|e| format!("import truncate: {e}"))?;

    let mut stats = ImportStats::default();
    for actor in snap.actors.into_values() {
        let key = actor.id.clone();
        tx.execute("INSERT INTO actors (id, data) VALUES ($1, $2)", &[&actor.id, &to_json(&actor)])
            .await.map_err(|e| format!("import actor {key}: {e}"))?;
        stats.actors += 1;
    }
    for account in snap.accounts.into_values() {
        let key = account.id.clone();
        tx.execute("INSERT INTO accounts (id, data) VALUES ($1, $2)", &[&account.id, &to_json(&account)])
            .await.map_err(|e| format!("import account {key}: {e}"))?;
        stats.accounts += 1;
    }
    for repo in snap.repos.into_values() {
        // A unique violation here means the snapshot still holds case-variant-duplicate repos under
        // one owner (the old loose create guard let those through); the (owner, name) in the error
        // tells the operator which pair to dedupe before re-importing.
        tx.execute(
            "INSERT INTO repos (id, owner, name, data) VALUES ($1, $2, $3, $4)",
            &[&repo.id, &repo.owner, &repo.name, &to_json(&repo)],
        )
        .await.map_err(|e| format!("import repo {}/{} (id {}): {e}", repo.owner, repo.name, repo.id))?;
        stats.repos += 1;
    }
    for issue in snap.issues {
        tx.execute(
            "INSERT INTO issues (repo, number, data) VALUES ($1, $2, $3)",
            &[&issue.repo, &(issue.number as i64), &to_json(&issue)],
        )
        .await.map_err(|e| format!("import issue {}#{}: {e}", issue.repo, issue.number))?;
        stats.issues += 1;
    }
    for pr in snap.prs {
        tx.execute(
            "INSERT INTO prs (repo, number, data) VALUES ($1, $2, $3)",
            &[&pr.repo, &(pr.number as i64), &to_json(&pr)],
        )
        .await.map_err(|e| format!("import pr {}#{}: {e}", pr.repo, pr.number))?;
        stats.prs += 1;
    }
    for review in snap.reviews {
        tx.execute(
            "INSERT INTO reviews (id, repo, data) VALUES ($1, $2, $3)",
            &[&review.id, &review.repo, &to_json(&review)],
        )
        .await.map_err(|e| format!("import review {} ({}): {e}", review.id, review.repo))?;
        stats.reviews += 1;
    }
    for comment in snap.comments {
        tx.execute(
            "INSERT INTO comments (id, repo, data) VALUES ($1, $2, $3)",
            &[&comment.id, &comment.repo, &to_json(&comment)],
        )
        .await.map_err(|e| format!("import comment {} ({}): {e}", comment.id, comment.repo))?;
        stats.comments += 1;
    }
    for session in snap.sessions {
        tx.execute(
            "INSERT INTO session_records (repo, change, data) VALUES ($1, $2, $3)",
            &[&session.repo, &session.change, &to_json(&session)],
        )
        .await.map_err(|e| format!("import session {}@{}: {e}", session.repo, session.change))?;
        stats.sessions += 1;
    }
    for project in snap.projects {
        tx.execute(
            "INSERT INTO projects (id, owner, data) VALUES ($1, $2, $3)",
            &[&project.id, &project.owner, &to_json(&project)],
        )
        .await.map_err(|e| format!("import project {} ({}): {e}", project.id, project.owner))?;
        stats.projects += 1;
    }
    for user in snap.users.into_values() {
        // Unique on lower(username): a collision names the duplicate login to dedupe.
        tx.execute(
            "INSERT INTO users (id, username, actor, data) VALUES ($1, $2, $3, $4)",
            &[&user.id, &user.username, &user.actor, &to_json(&user)],
        )
        .await.map_err(|e| format!("import user {} (username {}): {e}", user.id, user.username))?;
        stats.users += 1;
    }
    for team in snap.teams.into_values() {
        tx.execute(
            "INSERT INTO teams (id, account, data) VALUES ($1, $2, $3)",
            &[&team.id, &team.account, &to_json(&team)],
        )
        .await.map_err(|e| format!("import team {} ({}): {e}", team.id, team.account))?;
        stats.teams += 1;
    }
    for conn in snap.ai_conns {
        tx.execute(
            "INSERT INTO ai_connections (id, owner, data) VALUES ($1, $2, $3)",
            &[&conn.id, &conn.owner, &to_json(&conn)],
        )
        .await.map_err(|e| format!("import ai_connection {} ({}): {e}", conn.id, conn.owner))?;
        stats.ai_connections += 1;
    }
    for (owner, on) in snap.ai_rotate {
        tx.execute("INSERT INTO ai_rotate (owner, on_flag) VALUES ($1, $2)", &[&owner, &on])
            .await.map_err(|e| format!("import ai_rotate {owner}: {e}"))?;
        stats.ai_rotate += 1;
    }
    for (repo, rules) in snap.owners {
        tx.execute("INSERT INTO owners (repo, rules) VALUES ($1, $2)", &[&repo, &to_json(&rules)])
            .await.map_err(|e| format!("import owners {repo}: {e}"))?;
        stats.owners += 1;
    }
    // Usage is an absolute tally, not an increment — set it directly rather than via add_ai_usage.
    for (conn_id, usage) in snap.ai_usage {
        tx.execute(
            "INSERT INTO ai_usage (conn_id, input_tokens, output_tokens, cost_micros, runs, updated_unix) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            &[
                &conn_id,
                &(usage.input_tokens as i64),
                &(usage.output_tokens as i64),
                &(usage.cost_micros as i64),
                &(usage.runs as i64),
                &(usage.updated_unix as i64),
            ],
        )
        .await.map_err(|e| format!("import ai_usage {conn_id}: {e}"))?;
        stats.ai_usage += 1;
    }

    // Commit only after every row landed — a failure above returned Err with the transaction still
    // open, and dropping it here rolls back automatically.
    tx.commit().await.map_err(|e| format!("import commit: {e}"))?;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CloseReason, IssueStatus};

    fn issue(repo: &str, n: u64, title: &str) -> Issue {
        Issue {
            id: format!("i{n}"),
            repo: repo.into(),
            number: n,
            title: title.into(),
            body: String::new(),
            author: "you".into(),
            assignees: vec![],
            labels: vec![],
            projects: vec![],
            status: IssueStatus::Open,
            code_refs: vec![],
            referenced_actors: vec![],
            linked_prs: vec![],
            resolved_by: None,
            created_unix: 0,
            edited_unix: None,
        }
    }

    #[test]
    fn glob_match_dir_prefix() {
        // A bare directory or one with a trailing slash both mean "everything under it".
        assert!(glob_match("auth", "auth/login.rs"));
        assert!(glob_match("auth/", "auth/login.rs"));
        assert!(glob_match("auth/", "auth")); // the directory itself
        assert!(!glob_match("auth/", "authz/login.rs")); // not a prefix at a boundary
        assert!(!glob_match("auth/", "src/auth/login.rs")); // prefix only, not "anywhere"
    }

    #[test]
    fn glob_match_extension() {
        assert!(glob_match("*.rs", "src/main.rs"));
        assert!(!glob_match("*.rs", "src/main.py"));
    }

    #[test]
    fn glob_match_exact() {
        assert!(glob_match("src/main.rs", "src/main.rs"));
        assert!(!glob_match("src/main.rs", "src/lib.rs"));
    }

    #[test]
    fn glob_match_leading_globstar() {
        // `**/auth/**` matches an `auth` directory at ANY depth, including the root.
        assert!(glob_match("**/auth/**", "auth/login.rs"));
        assert!(glob_match("**/auth/**", "db/auth/login.rs"));
        assert!(glob_match("**/auth/**", "a/b/c/auth/x.rs"));
        assert!(!glob_match("**/auth/**", "src/main.rs"));
        assert!(!glob_match("**/migrations/**", "src/migration.rs")); // boundary, not substring
    }

    #[tokio::test]
    async fn file_store_persists_across_reopen() {
        let dir = std::env::temp_dir().join(format!("hull-store-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("store.json");

        {
            let s = FileStore::open(&path);
            s.put_issue(issue("acme/web", 1, "first")).await;
            s.put_issue(issue("acme/web", 2, "second")).await;
        }
        // reopen — data must survive
        let s2 = FileStore::open(&path);
        let issues = s2.issues("acme/web").await;
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[1].title, "second");
        assert!(s2.issues("other/repo").await.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn close_reason_roundtrips_through_snapshot() {
        let dir = std::env::temp_dir().join(format!("hull-store-test2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("store.json");
        let mut it = issue("r", 1, "x");
        it.status = IssueStatus::Closed { reason: CloseReason::Completed };
        FileStore::open(&path).put_issue(it).await;
        let reopened = FileStore::open(&path).issues("r").await;
        assert!(matches!(reopened[0].status, IssueStatus::Closed { reason: CloseReason::Completed }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ready_true_for_local_backends() {
        // InMemory has no external dependency — always ready.
        assert!(InMemory::new().ready().await);
        // FileStore is ready when its data directory is reachable (fresh install: the snapshot file
        // itself need not exist yet, only its parent dir).
        let dir = std::env::temp_dir().join(format!("hull-ready-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = FileStore::open(dir.join("store.json"));
        assert!(s.ready().await);
        // A backend rooted at a non-existent directory reports not-ready (writes would fail).
        let missing = FileStore::open(dir.join("nope").join("store.json"));
        assert!(!missing.ready().await);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── PostgresStore behavioral tests ────────────────────────────────────────────────────────
    // These need a real Postgres, so they're gated behind `HULL_TEST_DATABASE_URL` and SKIP (print
    // + early-return) when it's unset — CI here has no Postgres. A follow-up wires testcontainers /
    // a CI service. They assert the SAME behaviors as the InMemory/FileStore tests, so the trait's
    // semantics are proven identical across backends once a DB is available.

    /// Connect to the test Postgres, run migrations, and TRUNCATE all domain tables so each test
    /// starts clean. `None` (with a skip message) when `HULL_TEST_DATABASE_URL` is unset.
    async fn pg_test_store() -> Option<PostgresStore> {
        let url = std::env::var("HULL_TEST_DATABASE_URL").ok().filter(|u| !u.is_empty())?;
        let store = PostgresStore::connect(&url).await.expect("connect to HULL_TEST_DATABASE_URL");
        store
            .conn()
            .await
            .batch_execute(
                "TRUNCATE actors, accounts, repos, issues, prs, reviews, comments, session_records, \
                 projects, users, teams, ai_connections, ai_rotate, ai_usage, owners",
            )
            .await
            .expect("truncate test tables");
        Some(store)
    }

    macro_rules! pg_or_skip {
        ($name:literal) => {
            match pg_test_store().await {
                Some(s) => s,
                None => {
                    eprintln!("skipping {}: HULL_TEST_DATABASE_URL unset (no Postgres)", $name);
                    return;
                }
            }
        };
    }

    #[tokio::test]
    async fn pg_issue_crud_and_replace_and_update() {
        let s = pg_or_skip!("pg_issue_crud_and_replace_and_update");
        s.put_issue(issue("acme/web", 1, "first")).await;
        s.put_issue(issue("acme/web", 2, "second")).await;
        let issues = s.issues("acme/web").await;
        assert_eq!(issues.len(), 2);
        assert!(s.issues("other/repo").await.is_empty());

        // replace_issue: matches (repo, number), true iff one existed.
        let mut r = issue("acme/web", 2, "second-replaced");
        r.body = "b".into();
        assert!(s.replace_issue(r).await);
        assert!(!s.replace_issue(issue("acme/web", 99, "nope")).await);
        let after = s.issues("acme/web").await;
        assert_eq!(after.iter().find(|i| i.number == 2).unwrap().title, "second-replaced");

        // update_issue_content: None leaves a field unchanged; edited_unix is stamped.
        assert!(s.update_issue_content("acme/web", 1, Some("first-edited"), None, 42).await);
        assert!(!s.update_issue_content("acme/web", 999, Some("x"), None, 42).await);
        let i1 = s.issues("acme/web").await.into_iter().find(|i| i.number == 1).unwrap();
        assert_eq!(i1.title, "first-edited");
        assert_eq!(i1.body, ""); // body left unchanged (None)
        assert_eq!(i1.edited_unix, Some(42));
    }

    #[tokio::test]
    async fn pg_purge_and_rekey_repo_data() {
        let s = pg_or_skip!("pg_purge_and_rekey_repo_data");
        s.put_issue(issue("acme/web", 1, "a")).await;
        s.put_issue(issue("acme/web", 2, "b")).await;
        s.put_issue(issue("acme/api", 1, "keep")).await;
        s.set_owners("acme/web", vec![OwnerRule { glob: "*.rs".into(), owners: vec!["you".into()] }]).await;

        // rekey moves both the key column and the `repo` field inside the stored object.
        s.rekey_repo_data("acme/web", "acme/web2").await;
        assert!(s.issues("acme/web").await.is_empty());
        let moved = s.issues("acme/web2").await;
        assert_eq!(moved.len(), 2);
        assert!(moved.iter().all(|i| i.repo == "acme/web2"));
        assert_eq!(s.owners("acme/web2").await.len(), 1);
        assert!(s.owners("acme/web").await.is_empty());
        assert_eq!(s.issues("acme/api").await.len(), 1); // untouched

        // purge removes only the target repo's rows.
        s.purge_repo_data("acme/web2").await;
        assert!(s.issues("acme/web2").await.is_empty());
        assert!(s.owners("acme/web2").await.is_empty());
        assert_eq!(s.issues("acme/api").await.len(), 1);
    }

    #[tokio::test]
    async fn pg_session_upsert_and_ai_usage_increment() {
        let s = pg_or_skip!("pg_session_upsert_and_ai_usage_increment");
        // Latest write wins per (repo, change).
        let mut rec = SessionRecord { repo: "r".into(), change: "c1".into(), task: "t1".into(), model: String::new(), lesson: String::new(), tool_calls: 0, tokens_in: 0, tokens_out: 0 };
        s.put_session_record(rec.clone()).await;
        rec.task = "t2".into();
        s.put_session_record(rec).await;
        assert_eq!(s.session_record("r", "c1").await.unwrap().task, "t2");
        assert!(s.session_record("r", "missing").await.is_none());

        // add_ai_usage accumulates atomically.
        s.add_ai_usage("conn-1", 10, 5, 100, 1000).await;
        s.add_ai_usage("conn-1", 3, 2, 50, 2000).await;
        let u = s.ai_usage("conn-1").await;
        assert_eq!(u.input_tokens, 13);
        assert_eq!(u.output_tokens, 7);
        assert_eq!(u.cost_micros, 150);
        assert_eq!(u.runs, 2);
        assert_eq!(u.updated_unix, 2000);
        assert_eq!(s.ai_usage("absent").await.runs, 0); // default zeros
    }

    #[tokio::test]
    async fn pg_users_case_insensitive_lookup() {
        let s = pg_or_skip!("pg_users_case_insensitive_lookup");
        let u = User {
            id: "u1".into(),
            username: "Justin".into(),
            email: "j@example.com".into(),
            actor: "actor-1".into(),
            secret_key: "deadbeef".into(),
            passkeys: vec![],
            created_unix: 0,
            bio: String::new(),
        };
        s.put_user(u).await;
        assert_eq!(s.user("u1").await.unwrap().username, "Justin");
        assert_eq!(s.user_by_username("justin").await.unwrap().id, "u1"); // case-insensitive
        assert_eq!(s.user_by_actor("actor-1").await.unwrap().id, "u1");
        assert!(s.user_by_username("nobody").await.is_none());
    }
}
