//! Storage seam. The scaffold ships an in-memory store; M1+ swaps in a SQL-backed implementation
//! (accounts/issues/projects as relational rows) that references keel objects by content address.
//! Keeping this a trait means the server, tests, and the eventual SQL store all share one shape.

use crate::{Account, Actor, AiConnection, Comment, Issue, OwnerRule, Project, PullRequest, Repo, Review, SessionRecord, Team, User};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use r2d2_postgres::postgres::NoTls;
use r2d2_postgres::PostgresConnectionManager;

/// The persistence operations the server needs. Deliberately small for the scaffold — it grows per
/// milestone. All reads clone (cheap at this stage; the SQL store will stream).
pub trait Store: Send + Sync {
    fn put_actor(&self, actor: Actor);
    fn actor(&self, id: &str) -> Option<Actor>;
    fn actors(&self) -> Vec<Actor>;
    fn put_account(&self, account: Account);
    fn accounts(&self) -> Vec<Account>;
    fn put_repo(&self, repo: Repo);
    fn repos(&self) -> Vec<Repo>;
    /// Remove a repo record by id. Returns true if one was removed.
    fn remove_repo(&self, id: &str) -> bool;
    /// Remove every domain record (issues, PRs, reviews, comments, sessions, code-owner rules)
    /// whose repo key is `repo_key` (`<tenant>/<repo>`). Used when a repo is deleted.
    fn purge_repo_data(&self, repo_key: &str);
    /// Re-key every domain record (issues, PRs, reviews, comments, sessions, code-owner rules)
    /// from `old_key` to `new_key`. Used when a repo is renamed.
    fn rekey_repo_data(&self, old_key: &str, new_key: &str);
    fn put_issue(&self, issue: Issue);
    fn issues(&self, repo: &str) -> Vec<Issue>;
    /// Replace an existing issue, matched by `repo` + `number`. Returns true if one was replaced.
    fn replace_issue(&self, issue: Issue) -> bool;
    /// Update an issue's title and/or body (and stamp `edited_unix`), matched by `repo` + `number`.
    /// `None` leaves that field unchanged. Returns true if one was updated.
    fn update_issue_content(&self, repo: &str, number: u64, title: Option<&str>, body: Option<&str>, edited_unix: u64) -> bool;
    fn put_pr(&self, pr: PullRequest);
    fn prs(&self, repo: &str) -> Vec<PullRequest>;
    /// Replace an existing PR, matched by `repo` + `number`. Returns true if one was replaced.
    fn replace_pr(&self, pr: PullRequest) -> bool;
    fn put_review(&self, review: Review);
    fn reviews(&self, repo: &str) -> Vec<Review>;
    fn put_comment(&self, comment: Comment);
    fn comments(&self, repo: &str) -> Vec<Comment>;
    /// Delete a comment by id. Returns true if one was removed.
    fn remove_comment(&self, repo: &str, id: &str) -> bool;
    /// Update a comment's body (and stamp `edited_unix`) by id. Returns true if one was updated.
    fn update_comment_body(&self, repo: &str, id: &str, body: &str, edited_unix: u64) -> bool;
    /// Associate an ingested keel session with a change (latest write wins per change).
    fn put_session_record(&self, record: SessionRecord);
    fn session_record(&self, repo: &str, change: &str) -> Option<SessionRecord>;
    fn put_project(&self, project: Project);
    fn projects(&self, owner: &str) -> Vec<Project>;
    // ── AI connections (per account/org: multiple backends, optional rotation) ──
    fn put_ai_connection(&self, conn: AiConnection);
    fn ai_connections(&self, owner: &str) -> Vec<AiConnection>;
    fn remove_ai_connection(&self, owner: &str, id: &str) -> bool;
    fn set_ai_rotate(&self, owner: &str, on: bool);
    fn ai_rotate(&self, owner: &str) -> bool;
    /// Add one agent run's token usage to a connection's rolling tally.
    fn add_ai_usage(&self, conn_id: &str, input: u64, output: u64, cost_micros: u64, at_unix: u64);
    /// A connection's accumulated usage (default zeros if none).
    fn ai_usage(&self, conn_id: &str) -> crate::AiUsage;
    /// Set a repo's code-owner rules (replaces the existing set).
    fn set_owners(&self, repo: &str, rules: Vec<OwnerRule>);
    fn owners(&self, repo: &str) -> Vec<OwnerRule>;
    // ── hosted-account identities (passkey login) ──
    fn put_user(&self, user: User);
    fn user(&self, id: &str) -> Option<User>;
    fn user_by_username(&self, username: &str) -> Option<User>;
    fn user_by_actor(&self, actor: &str) -> Option<User>;
    fn users(&self) -> Vec<User>;
    // ── org teams ──
    fn put_team(&self, team: Team);
    fn team(&self, id: &str) -> Option<Team>;
    fn teams(&self, account: &str) -> Vec<Team>;
    fn delete_team(&self, id: &str);
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

impl Store for InMemory {
    fn put_actor(&self, actor: Actor) {
        self.actors.write().unwrap().insert(actor.id.clone(), actor);
    }
    fn actor(&self, id: &str) -> Option<Actor> {
        self.actors.read().unwrap().get(id).cloned()
    }
    fn actors(&self) -> Vec<Actor> {
        self.actors.read().unwrap().values().cloned().collect()
    }
    fn put_account(&self, account: Account) {
        self.accounts.write().unwrap().insert(account.id.clone(), account);
    }
    fn accounts(&self) -> Vec<Account> {
        self.accounts.read().unwrap().values().cloned().collect()
    }
    fn put_repo(&self, repo: Repo) {
        self.repos.write().unwrap().insert(repo.id.clone(), repo);
    }
    fn repos(&self) -> Vec<Repo> {
        self.repos.read().unwrap().values().cloned().collect()
    }
    fn remove_repo(&self, id: &str) -> bool {
        self.repos.write().unwrap().remove(id).is_some()
    }
    fn purge_repo_data(&self, repo_key: &str) {
        self.issues.write().unwrap().retain(|i| i.repo != repo_key);
        self.prs.write().unwrap().retain(|p| p.repo != repo_key);
        self.reviews.write().unwrap().retain(|r| r.repo != repo_key);
        self.comments.write().unwrap().retain(|c| c.repo != repo_key);
        self.sessions.write().unwrap().retain(|s| s.repo != repo_key);
        self.owners.write().unwrap().remove(repo_key);
    }
    fn rekey_repo_data(&self, old_key: &str, new_key: &str) {
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
    fn put_issue(&self, issue: Issue) {
        self.issues.write().unwrap().push(issue);
    }
    fn issues(&self, repo: &str) -> Vec<Issue> {
        self.issues.read().unwrap().iter().filter(|i| i.repo == repo).cloned().collect()
    }
    fn replace_issue(&self, issue: Issue) -> bool {
        let mut g = self.issues.write().unwrap();
        match g.iter_mut().find(|i| i.repo == issue.repo && i.number == issue.number) {
            Some(slot) => {
                *slot = issue;
                true
            }
            None => false,
        }
    }
    fn update_issue_content(&self, repo: &str, number: u64, title: Option<&str>, body: Option<&str>, edited_unix: u64) -> bool {
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
    fn put_pr(&self, pr: PullRequest) {
        self.prs.write().unwrap().push(pr);
    }
    fn prs(&self, repo: &str) -> Vec<PullRequest> {
        self.prs.read().unwrap().iter().filter(|p| p.repo == repo).cloned().collect()
    }
    fn replace_pr(&self, pr: PullRequest) -> bool {
        let mut g = self.prs.write().unwrap();
        match g.iter_mut().find(|p| p.repo == pr.repo && p.number == pr.number) {
            Some(slot) => {
                *slot = pr;
                true
            }
            None => false,
        }
    }
    fn put_review(&self, review: Review) {
        self.reviews.write().unwrap().push(review);
    }
    fn reviews(&self, repo: &str) -> Vec<Review> {
        self.reviews.read().unwrap().iter().filter(|r| r.repo == repo).cloned().collect()
    }
    fn put_comment(&self, comment: Comment) {
        self.comments.write().unwrap().push(comment);
    }
    fn comments(&self, repo: &str) -> Vec<Comment> {
        self.comments.read().unwrap().iter().filter(|c| c.repo == repo).cloned().collect()
    }
    fn remove_comment(&self, repo: &str, id: &str) -> bool {
        let mut g = self.comments.write().unwrap();
        let before = g.len();
        g.retain(|c| !(c.repo == repo && c.id == id));
        g.len() != before
    }
    fn update_comment_body(&self, repo: &str, id: &str, body: &str, edited_unix: u64) -> bool {
        let mut g = self.comments.write().unwrap();
        if let Some(c) = g.iter_mut().find(|c| c.repo == repo && c.id == id) {
            c.body = body.to_string();
            c.edited_unix = Some(edited_unix);
            true
        } else {
            false
        }
    }
    fn put_session_record(&self, record: SessionRecord) {
        let mut g = self.sessions.write().unwrap();
        g.retain(|s| !(s.repo == record.repo && s.change == record.change));
        g.push(record);
    }
    fn session_record(&self, repo: &str, change: &str) -> Option<SessionRecord> {
        self.sessions.read().unwrap().iter().find(|s| s.repo == repo && s.change == change).cloned()
    }
    fn put_project(&self, project: Project) {
        self.projects.write().unwrap().push(project);
    }
    fn projects(&self, owner: &str) -> Vec<Project> {
        self.projects.read().unwrap().iter().filter(|p| p.owner == owner).cloned().collect()
    }
    fn put_ai_connection(&self, conn: AiConnection) {
        self.ai_conns.write().unwrap().push(conn);
    }
    fn ai_connections(&self, owner: &str) -> Vec<AiConnection> {
        self.ai_conns.read().unwrap().iter().filter(|c| c.owner == owner).cloned().collect()
    }
    fn remove_ai_connection(&self, owner: &str, id: &str) -> bool {
        let mut g = self.ai_conns.write().unwrap();
        let n = g.len();
        g.retain(|c| !(c.owner == owner && c.id == id));
        g.len() != n
    }
    fn set_ai_rotate(&self, owner: &str, on: bool) {
        self.ai_rotate.write().unwrap().insert(owner.to_string(), on);
    }
    fn ai_rotate(&self, owner: &str) -> bool {
        self.ai_rotate.read().unwrap().get(owner).copied().unwrap_or(false)
    }
    fn add_ai_usage(&self, conn_id: &str, input: u64, output: u64, cost_micros: u64, at_unix: u64) {
        let mut g = self.ai_usage.write().unwrap();
        let u = g.entry(conn_id.to_string()).or_default();
        u.input_tokens += input;
        u.output_tokens += output;
        u.cost_micros += cost_micros;
        u.runs += 1;
        u.updated_unix = at_unix;
    }
    fn ai_usage(&self, conn_id: &str) -> crate::AiUsage {
        self.ai_usage.read().unwrap().get(conn_id).cloned().unwrap_or_default()
    }
    fn set_owners(&self, repo: &str, rules: Vec<OwnerRule>) {
        self.owners.write().unwrap().insert(repo.to_string(), rules);
    }
    fn owners(&self, repo: &str) -> Vec<OwnerRule> {
        self.owners.read().unwrap().get(repo).cloned().unwrap_or_default()
    }
    fn put_user(&self, user: User) {
        self.users.write().unwrap().insert(user.id.clone(), user);
    }
    fn user(&self, id: &str) -> Option<User> {
        self.users.read().unwrap().get(id).cloned()
    }
    fn user_by_username(&self, username: &str) -> Option<User> {
        self.users.read().unwrap().values().find(|u| u.username.eq_ignore_ascii_case(username)).cloned()
    }
    fn user_by_actor(&self, actor: &str) -> Option<User> {
        self.users.read().unwrap().values().find(|u| u.actor == actor).cloned()
    }
    fn users(&self) -> Vec<User> {
        self.users.read().unwrap().values().cloned().collect()
    }
    fn put_team(&self, team: Team) {
        self.teams.write().unwrap().insert(team.id.clone(), team);
    }
    fn team(&self, id: &str) -> Option<Team> {
        self.teams.read().unwrap().get(id).cloned()
    }
    fn teams(&self, account: &str) -> Vec<Team> {
        self.teams.read().unwrap().values().filter(|t| t.account == account).cloned().collect()
    }
    fn delete_team(&self, id: &str) {
        self.teams.write().unwrap().remove(id);
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

    /// Persist the current snapshot atomically. A write failure is logged, not fatal — the in-memory
    /// state stays correct for this process.
    fn save(&self, snap: &Snapshot) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = self.path.with_extension("json.tmp");
        match serde_json::to_vec_pretty(snap) {
            Ok(bytes) => {
                if std::fs::write(&tmp, &bytes).and_then(|_| std::fs::rename(&tmp, &self.path)).is_err() {
                    eprintln!("hull: failed to persist store to {}", self.path.display());
                }
            }
            Err(e) => eprintln!("hull: failed to serialize store: {e}"),
        }
    }

    fn mutate(&self, f: impl FnOnce(&mut Snapshot)) {
        let mut g = self.inner.write().unwrap();
        f(&mut g);
        self.save(&g);
    }
}

impl Store for FileStore {
    fn put_actor(&self, actor: Actor) {
        self.mutate(|s| {
            s.actors.insert(actor.id.clone(), actor);
        });
    }
    fn actor(&self, id: &str) -> Option<Actor> {
        self.inner.read().unwrap().actors.get(id).cloned()
    }
    fn actors(&self) -> Vec<Actor> {
        self.inner.read().unwrap().actors.values().cloned().collect()
    }
    fn put_account(&self, account: Account) {
        self.mutate(|s| {
            s.accounts.insert(account.id.clone(), account);
        });
    }
    fn accounts(&self) -> Vec<Account> {
        self.inner.read().unwrap().accounts.values().cloned().collect()
    }
    fn put_repo(&self, repo: Repo) {
        self.mutate(|s| {
            s.repos.insert(repo.id.clone(), repo);
        });
    }
    fn repos(&self) -> Vec<Repo> {
        self.inner.read().unwrap().repos.values().cloned().collect()
    }
    fn remove_repo(&self, id: &str) -> bool {
        let mut removed = false;
        self.mutate(|s| removed = s.repos.remove(id).is_some());
        removed
    }
    fn purge_repo_data(&self, repo_key: &str) {
        self.mutate(|s| {
            s.issues.retain(|i| i.repo != repo_key);
            s.prs.retain(|p| p.repo != repo_key);
            s.reviews.retain(|r| r.repo != repo_key);
            s.comments.retain(|c| c.repo != repo_key);
            s.sessions.retain(|x| x.repo != repo_key);
            s.owners.remove(repo_key);
        });
    }
    fn rekey_repo_data(&self, old_key: &str, new_key: &str) {
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
    fn put_issue(&self, issue: Issue) {
        self.mutate(|s| s.issues.push(issue));
    }
    fn issues(&self, repo: &str) -> Vec<Issue> {
        self.inner.read().unwrap().issues.iter().filter(|i| i.repo == repo).cloned().collect()
    }
    fn replace_issue(&self, issue: Issue) -> bool {
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
    fn update_issue_content(&self, repo: &str, number: u64, title: Option<&str>, body: Option<&str>, edited_unix: u64) -> bool {
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
    fn put_pr(&self, pr: PullRequest) {
        self.mutate(|s| s.prs.push(pr));
    }
    fn prs(&self, repo: &str) -> Vec<PullRequest> {
        self.inner.read().unwrap().prs.iter().filter(|p| p.repo == repo).cloned().collect()
    }
    fn replace_pr(&self, pr: PullRequest) -> bool {
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
    fn put_review(&self, review: Review) {
        self.mutate(|s| s.reviews.push(review));
    }
    fn reviews(&self, repo: &str) -> Vec<Review> {
        self.inner.read().unwrap().reviews.iter().filter(|r| r.repo == repo).cloned().collect()
    }
    fn put_comment(&self, comment: Comment) {
        self.mutate(|s| s.comments.push(comment));
    }
    fn comments(&self, repo: &str) -> Vec<Comment> {
        self.inner.read().unwrap().comments.iter().filter(|c| c.repo == repo).cloned().collect()
    }
    fn remove_comment(&self, repo: &str, id: &str) -> bool {
        let mut removed = false;
        self.mutate(|s| { let before = s.comments.len(); s.comments.retain(|c| !(c.repo == repo && c.id == id)); removed = s.comments.len() != before; });
        removed
    }
    fn update_comment_body(&self, repo: &str, id: &str, body: &str, edited_unix: u64) -> bool {
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
    fn put_session_record(&self, record: SessionRecord) {
        self.mutate(|s| {
            s.sessions.retain(|x| !(x.repo == record.repo && x.change == record.change));
            s.sessions.push(record);
        });
    }
    fn session_record(&self, repo: &str, change: &str) -> Option<SessionRecord> {
        self.inner.read().unwrap().sessions.iter().find(|s| s.repo == repo && s.change == change).cloned()
    }
    fn put_project(&self, project: Project) {
        self.mutate(|s| s.projects.push(project));
    }
    fn projects(&self, owner: &str) -> Vec<Project> {
        self.inner.read().unwrap().projects.iter().filter(|p| p.owner == owner).cloned().collect()
    }
    fn put_ai_connection(&self, conn: AiConnection) {
        self.mutate(|s| s.ai_conns.push(conn));
    }
    fn ai_connections(&self, owner: &str) -> Vec<AiConnection> {
        self.inner.read().unwrap().ai_conns.iter().filter(|c| c.owner == owner).cloned().collect()
    }
    fn remove_ai_connection(&self, owner: &str, id: &str) -> bool {
        let mut removed = false;
        self.mutate(|s| { let n = s.ai_conns.len(); s.ai_conns.retain(|c| !(c.owner == owner && c.id == id)); removed = s.ai_conns.len() != n; });
        removed
    }
    fn set_ai_rotate(&self, owner: &str, on: bool) {
        self.mutate(|s| { s.ai_rotate.insert(owner.to_string(), on); });
    }
    fn ai_rotate(&self, owner: &str) -> bool {
        self.inner.read().unwrap().ai_rotate.get(owner).copied().unwrap_or(false)
    }
    fn add_ai_usage(&self, conn_id: &str, input: u64, output: u64, cost_micros: u64, at_unix: u64) {
        self.mutate(|s| {
            let u = s.ai_usage.entry(conn_id.to_string()).or_default();
            u.input_tokens += input;
            u.output_tokens += output;
            u.cost_micros += cost_micros;
            u.runs += 1;
            u.updated_unix = at_unix;
        });
    }
    fn ai_usage(&self, conn_id: &str) -> crate::AiUsage {
        self.inner.read().unwrap().ai_usage.get(conn_id).cloned().unwrap_or_default()
    }
    fn set_owners(&self, repo: &str, rules: Vec<OwnerRule>) {
        self.mutate(|s| {
            s.owners.insert(repo.to_string(), rules);
        });
    }
    fn owners(&self, repo: &str) -> Vec<OwnerRule> {
        self.inner.read().unwrap().owners.get(repo).cloned().unwrap_or_default()
    }
    fn put_user(&self, user: User) {
        self.mutate(|s| {
            s.users.insert(user.id.clone(), user);
        });
    }
    fn user(&self, id: &str) -> Option<User> {
        self.inner.read().unwrap().users.get(id).cloned()
    }
    fn user_by_username(&self, username: &str) -> Option<User> {
        self.inner.read().unwrap().users.values().find(|u| u.username.eq_ignore_ascii_case(username)).cloned()
    }
    fn user_by_actor(&self, actor: &str) -> Option<User> {
        self.inner.read().unwrap().users.values().find(|u| u.actor == actor).cloned()
    }
    fn users(&self) -> Vec<User> {
        self.inner.read().unwrap().users.values().cloned().collect()
    }
    fn put_team(&self, team: Team) {
        self.mutate(|s| {
            s.teams.insert(team.id.clone(), team);
        });
    }
    fn team(&self, id: &str) -> Option<Team> {
        self.inner.read().unwrap().teams.get(id).cloned()
    }
    fn teams(&self, account: &str) -> Vec<Team> {
        self.inner.read().unwrap().teams.values().filter(|t| t.account == account).cloned().collect()
    }
    fn delete_team(&self, id: &str) {
        self.mutate(|s| {
            s.teams.remove(id);
        });
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// Postgres-backed [`Store`] (M1).
//
// The trait stays SYNCHRONOUS; concurrency comes from an r2d2 blocking connection pool over the
// synchronous `postgres` driver. Every method checks out a pooled connection and runs blocking SQL.
// Each domain object is stored as one row: its full serialized form in a JSONB `data` column, with
// the fields the trait matches/filters on lifted into indexed key columns. Reads deserialize `data`
// back into the exact domain type, so this is a faithful mirror of `Snapshot`/`InMemory`.
//
// Error policy: a SQL/connection failure inside a trait method is exceptional (the DB being down is
// not a normal condition) and panics via `.expect(...)`, consistent with `InMemory` unwrapping its
// locks. `connect` returns `Result` so wiring can surface a bad URL / failed migration at startup.
// ─────────────────────────────────────────────────────────────────────────────────────────────

type PgManager = PostgresConnectionManager<NoTls>;
type PgPool = r2d2::Pool<PgManager>;
type PgConn = r2d2::PooledConnection<PgManager>;

/// Embedded, ordered schema migrations. Index `i` is version `i + 1`; applied once, tracked in
/// `_hull_schema_version`. Append new files here (never edit an applied one) to evolve the schema.
const MIGRATIONS: &[&str] = &[include_str!("migrations/0001_init.sql")];

/// A durable [`Store`] backed by Postgres. Chosen at runtime when `HULL_DATABASE_URL` is set; the
/// default (unset) path keeps [`FileStore`]. Content/provenance still live in keel — this persists
/// only Hull's relational domain objects, referencing keel objects by content address.
pub struct PostgresStore {
    pool: PgPool,
}

impl PostgresStore {
    /// Connect (building the pool), then run migrations. `database_url` is a standard libpq URL,
    /// e.g. `postgres://user:pass@host:5432/hull`. Returns an error string on a bad URL, an
    /// unreachable server, or a failed migration — the caller decides whether that's fatal.
    pub fn connect(database_url: &str) -> Result<Self, String> {
        let config = database_url
            .parse::<r2d2_postgres::postgres::Config>()
            .map_err(|e| format!("hull: invalid HULL_DATABASE_URL: {e}"))?;
        let manager = PostgresConnectionManager::new(config, NoTls);
        let pool = r2d2::Pool::builder()
            .build(manager)
            .map_err(|e| format!("hull: failed to connect to Postgres: {e}"))?;
        let store = PostgresStore { pool };
        store.migrate().map_err(|e| format!("hull: Postgres migration failed: {e}"))?;
        Ok(store)
    }

    /// Check out a pooled connection. Panics if the pool can't hand one out (server gone) — see the
    /// module error policy.
    fn conn(&self) -> PgConn {
        self.pool.get().expect("hull: postgres connection pool exhausted or server unreachable")
    }

    /// Apply any migrations not yet recorded in `_hull_schema_version`, each in its own transaction.
    fn migrate(&self) -> Result<(), r2d2_postgres::postgres::Error> {
        let mut c = self.conn();
        c.batch_execute(
            "CREATE TABLE IF NOT EXISTS _hull_schema_version (version INT PRIMARY KEY, applied_unix BIGINT NOT NULL)",
        )?;
        let current: i32 = c.query_one("SELECT COALESCE(MAX(version), 0) FROM _hull_schema_version", &[])?.get(0);
        let now = now_unix();
        for (idx, sql) in MIGRATIONS.iter().enumerate() {
            let version = idx as i32 + 1;
            if version > current {
                let mut tx = c.transaction()?;
                tx.batch_execute(sql)?;
                tx.execute("INSERT INTO _hull_schema_version (version, applied_unix) VALUES ($1, $2)", &[&version, &now])?;
                tx.commit()?;
            }
        }
        Ok(())
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

impl Store for PostgresStore {
    fn put_actor(&self, actor: Actor) {
        self.conn()
            .execute(
                "INSERT INTO actors (id, data) VALUES ($1, $2) ON CONFLICT (id) DO UPDATE SET data = EXCLUDED.data",
                &[&actor.id, &to_json(&actor)],
            )
            .expect("put_actor");
    }
    fn actor(&self, id: &str) -> Option<Actor> {
        self.conn().query_opt("SELECT data FROM actors WHERE id = $1", &[&id]).expect("actor").map(|r| from_json(r.get(0)))
    }
    fn actors(&self) -> Vec<Actor> {
        self.conn().query("SELECT data FROM actors", &[]).expect("actors").into_iter().map(|r| from_json(r.get(0))).collect()
    }
    fn put_account(&self, account: Account) {
        self.conn()
            .execute(
                "INSERT INTO accounts (id, data) VALUES ($1, $2) ON CONFLICT (id) DO UPDATE SET data = EXCLUDED.data",
                &[&account.id, &to_json(&account)],
            )
            .expect("put_account");
    }
    fn accounts(&self) -> Vec<Account> {
        self.conn().query("SELECT data FROM accounts", &[]).expect("accounts").into_iter().map(|r| from_json(r.get(0))).collect()
    }
    fn put_repo(&self, repo: Repo) {
        self.conn()
            .execute(
                "INSERT INTO repos (id, owner, name, data) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (id) DO UPDATE SET owner = EXCLUDED.owner, name = EXCLUDED.name, data = EXCLUDED.data",
                &[&repo.id, &repo.owner, &repo.name, &to_json(&repo)],
            )
            .expect("put_repo");
    }
    fn repos(&self) -> Vec<Repo> {
        self.conn().query("SELECT data FROM repos", &[]).expect("repos").into_iter().map(|r| from_json(r.get(0))).collect()
    }
    fn remove_repo(&self, id: &str) -> bool {
        self.conn().execute("DELETE FROM repos WHERE id = $1", &[&id]).expect("remove_repo") > 0
    }
    fn purge_repo_data(&self, repo_key: &str) {
        let mut c = self.conn();
        let mut tx = c.transaction().expect("purge tx");
        for table in ["issues", "prs", "reviews", "comments", "session_records", "owners"] {
            tx.execute(&format!("DELETE FROM {table} WHERE repo = $1"), &[&repo_key]).expect("purge delete");
        }
        tx.commit().expect("purge commit");
    }
    fn rekey_repo_data(&self, old_key: &str, new_key: &str) {
        let mut c = self.conn();
        let mut tx = c.transaction().expect("rekey tx");
        // These carry `repo` both as a key column AND inside `data` — move both so the stored object
        // matches the in-memory rekey (which rewrites the struct's `repo` field).
        for table in ["issues", "prs", "reviews", "comments", "session_records"] {
            tx.execute(
                &format!("UPDATE {table} SET repo = $1, data = jsonb_set(data, '{{repo}}', to_jsonb($1::text)) WHERE repo = $2"),
                &[&new_key, &old_key],
            )
            .expect("rekey update");
        }
        // Owners are keyed only by repo (no `repo` field inside). Mirror the in-memory move: only if
        // the old key exists, and overwrite any rules already at the new key.
        if let Some(row) = tx.query_opt("SELECT rules FROM owners WHERE repo = $1", &[&old_key]).expect("rekey owners read") {
            let rules: serde_json::Value = row.get(0);
            tx.execute("DELETE FROM owners WHERE repo = $1 OR repo = $2", &[&old_key, &new_key]).expect("rekey owners clear");
            tx.execute("INSERT INTO owners (repo, rules) VALUES ($1, $2)", &[&new_key, &rules]).expect("rekey owners set");
        }
        tx.commit().expect("rekey commit");
    }
    fn put_issue(&self, issue: Issue) {
        self.conn()
            .execute("INSERT INTO issues (repo, number, data) VALUES ($1, $2, $3)", &[&issue.repo, &(issue.number as i64), &to_json(&issue)])
            .expect("put_issue");
    }
    fn issues(&self, repo: &str) -> Vec<Issue> {
        self.conn().query("SELECT data FROM issues WHERE repo = $1", &[&repo]).expect("issues").into_iter().map(|r| from_json(r.get(0))).collect()
    }
    fn replace_issue(&self, issue: Issue) -> bool {
        // UPDATE … WHERE (repo, number) RETURNING — no read-your-write-in-memory assumption. The
        // match key is unchanged, so the `repo`/`number` columns stay consistent with `data`.
        self.conn()
            .execute(
                "UPDATE issues SET data = $1 WHERE repo = $2 AND number = $3",
                &[&to_json(&issue), &issue.repo, &(issue.number as i64)],
            )
            .expect("replace_issue")
            > 0
    }
    fn update_issue_content(&self, repo: &str, number: u64, title: Option<&str>, body: Option<&str>, edited_unix: u64) -> bool {
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
        self.conn()
            .execute(
                "UPDATE issues SET data = data || $1 WHERE repo = $2 AND number = $3",
                &[&serde_json::Value::Object(patch), &repo, &(number as i64)],
            )
            .expect("update_issue_content")
            > 0
    }
    fn put_pr(&self, pr: PullRequest) {
        self.conn()
            .execute("INSERT INTO prs (repo, number, data) VALUES ($1, $2, $3)", &[&pr.repo, &(pr.number as i64), &to_json(&pr)])
            .expect("put_pr");
    }
    fn prs(&self, repo: &str) -> Vec<PullRequest> {
        self.conn().query("SELECT data FROM prs WHERE repo = $1", &[&repo]).expect("prs").into_iter().map(|r| from_json(r.get(0))).collect()
    }
    fn replace_pr(&self, pr: PullRequest) -> bool {
        self.conn()
            .execute("UPDATE prs SET data = $1 WHERE repo = $2 AND number = $3", &[&to_json(&pr), &pr.repo, &(pr.number as i64)])
            .expect("replace_pr")
            > 0
    }
    fn put_review(&self, review: Review) {
        self.conn()
            .execute("INSERT INTO reviews (id, repo, data) VALUES ($1, $2, $3)", &[&review.id, &review.repo, &to_json(&review)])
            .expect("put_review");
    }
    fn reviews(&self, repo: &str) -> Vec<Review> {
        self.conn().query("SELECT data FROM reviews WHERE repo = $1", &[&repo]).expect("reviews").into_iter().map(|r| from_json(r.get(0))).collect()
    }
    fn put_comment(&self, comment: Comment) {
        self.conn()
            .execute("INSERT INTO comments (id, repo, data) VALUES ($1, $2, $3)", &[&comment.id, &comment.repo, &to_json(&comment)])
            .expect("put_comment");
    }
    fn comments(&self, repo: &str) -> Vec<Comment> {
        self.conn().query("SELECT data FROM comments WHERE repo = $1", &[&repo]).expect("comments").into_iter().map(|r| from_json(r.get(0))).collect()
    }
    fn remove_comment(&self, repo: &str, id: &str) -> bool {
        self.conn().execute("DELETE FROM comments WHERE repo = $1 AND id = $2", &[&repo, &id]).expect("remove_comment") > 0
    }
    fn update_comment_body(&self, repo: &str, id: &str, body: &str, edited_unix: u64) -> bool {
        let patch = serde_json::json!({ "body": body, "edited_unix": edited_unix });
        self.conn()
            .execute("UPDATE comments SET data = data || $1 WHERE repo = $2 AND id = $3", &[&patch, &repo, &id])
            .expect("update_comment_body")
            > 0
    }
    fn put_session_record(&self, record: SessionRecord) {
        // Latest write wins per (repo, change) — upsert on the unique key.
        self.conn()
            .execute(
                "INSERT INTO session_records (repo, change, data) VALUES ($1, $2, $3) \
                 ON CONFLICT (repo, change) DO UPDATE SET data = EXCLUDED.data",
                &[&record.repo, &record.change, &to_json(&record)],
            )
            .expect("put_session_record");
    }
    fn session_record(&self, repo: &str, change: &str) -> Option<SessionRecord> {
        self.conn()
            .query_opt("SELECT data FROM session_records WHERE repo = $1 AND change = $2", &[&repo, &change])
            .expect("session_record")
            .map(|r| from_json(r.get(0)))
    }
    fn put_project(&self, project: Project) {
        self.conn()
            .execute("INSERT INTO projects (id, owner, data) VALUES ($1, $2, $3)", &[&project.id, &project.owner, &to_json(&project)])
            .expect("put_project");
    }
    fn projects(&self, owner: &str) -> Vec<Project> {
        self.conn().query("SELECT data FROM projects WHERE owner = $1", &[&owner]).expect("projects").into_iter().map(|r| from_json(r.get(0))).collect()
    }
    fn put_ai_connection(&self, conn: AiConnection) {
        self.conn()
            .execute("INSERT INTO ai_connections (id, owner, data) VALUES ($1, $2, $3)", &[&conn.id, &conn.owner, &to_json(&conn)])
            .expect("put_ai_connection");
    }
    fn ai_connections(&self, owner: &str) -> Vec<AiConnection> {
        self.conn().query("SELECT data FROM ai_connections WHERE owner = $1", &[&owner]).expect("ai_connections").into_iter().map(|r| from_json(r.get(0))).collect()
    }
    fn remove_ai_connection(&self, owner: &str, id: &str) -> bool {
        self.conn().execute("DELETE FROM ai_connections WHERE owner = $1 AND id = $2", &[&owner, &id]).expect("remove_ai_connection") > 0
    }
    fn set_ai_rotate(&self, owner: &str, on: bool) {
        self.conn()
            .execute(
                "INSERT INTO ai_rotate (owner, on_flag) VALUES ($1, $2) ON CONFLICT (owner) DO UPDATE SET on_flag = EXCLUDED.on_flag",
                &[&owner, &on],
            )
            .expect("set_ai_rotate");
    }
    fn ai_rotate(&self, owner: &str) -> bool {
        self.conn()
            .query_opt("SELECT on_flag FROM ai_rotate WHERE owner = $1", &[&owner])
            .expect("ai_rotate")
            .map(|r| r.get(0))
            .unwrap_or(false)
    }
    fn add_ai_usage(&self, conn_id: &str, input: u64, output: u64, cost_micros: u64, at_unix: u64) {
        // Atomic accumulate (input = input + delta, runs = runs + 1) — never read-modify-write.
        self.conn()
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
            .expect("add_ai_usage");
    }
    fn ai_usage(&self, conn_id: &str) -> crate::AiUsage {
        self.conn()
            .query_opt(
                "SELECT input_tokens, output_tokens, cost_micros, runs, updated_unix FROM ai_usage WHERE conn_id = $1",
                &[&conn_id],
            )
            .expect("ai_usage")
            .map(|r| crate::AiUsage {
                input_tokens: r.get::<_, i64>(0) as u64,
                output_tokens: r.get::<_, i64>(1) as u64,
                cost_micros: r.get::<_, i64>(2) as u64,
                runs: r.get::<_, i64>(3) as u64,
                updated_unix: r.get::<_, i64>(4) as u64,
            })
            .unwrap_or_default()
    }
    fn set_owners(&self, repo: &str, rules: Vec<OwnerRule>) {
        self.conn()
            .execute(
                "INSERT INTO owners (repo, rules) VALUES ($1, $2) ON CONFLICT (repo) DO UPDATE SET rules = EXCLUDED.rules",
                &[&repo, &to_json(&rules)],
            )
            .expect("set_owners");
    }
    fn owners(&self, repo: &str) -> Vec<OwnerRule> {
        self.conn()
            .query_opt("SELECT rules FROM owners WHERE repo = $1", &[&repo])
            .expect("owners")
            .map(|r| from_json(r.get(0)))
            .unwrap_or_default()
    }
    fn put_user(&self, user: User) {
        self.conn()
            .execute(
                "INSERT INTO users (id, username, actor, data) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (id) DO UPDATE SET username = EXCLUDED.username, actor = EXCLUDED.actor, data = EXCLUDED.data",
                &[&user.id, &user.username, &user.actor, &to_json(&user)],
            )
            .expect("put_user");
    }
    fn user(&self, id: &str) -> Option<User> {
        self.conn().query_opt("SELECT data FROM users WHERE id = $1", &[&id]).expect("user").map(|r| from_json(r.get(0)))
    }
    fn user_by_username(&self, username: &str) -> Option<User> {
        self.conn()
            .query_opt("SELECT data FROM users WHERE lower(username) = lower($1)", &[&username])
            .expect("user_by_username")
            .map(|r| from_json(r.get(0)))
    }
    fn user_by_actor(&self, actor: &str) -> Option<User> {
        self.conn()
            .query_opt("SELECT data FROM users WHERE actor = $1 LIMIT 1", &[&actor])
            .expect("user_by_actor")
            .map(|r| from_json(r.get(0)))
    }
    fn users(&self) -> Vec<User> {
        self.conn().query("SELECT data FROM users", &[]).expect("users").into_iter().map(|r| from_json(r.get(0))).collect()
    }
    fn put_team(&self, team: Team) {
        self.conn()
            .execute(
                "INSERT INTO teams (id, account, data) VALUES ($1, $2, $3) \
                 ON CONFLICT (id) DO UPDATE SET account = EXCLUDED.account, data = EXCLUDED.data",
                &[&team.id, &team.account, &to_json(&team)],
            )
            .expect("put_team");
    }
    fn team(&self, id: &str) -> Option<Team> {
        self.conn().query_opt("SELECT data FROM teams WHERE id = $1", &[&id]).expect("team").map(|r| from_json(r.get(0)))
    }
    fn teams(&self, account: &str) -> Vec<Team> {
        self.conn().query("SELECT data FROM teams WHERE account = $1", &[&account]).expect("teams").into_iter().map(|r| from_json(r.get(0))).collect()
    }
    fn delete_team(&self, id: &str) {
        self.conn().execute("DELETE FROM teams WHERE id = $1", &[&id]).expect("delete_team");
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
pub fn import_store_json(pg: &PostgresStore, path: &Path) -> Result<ImportStats, String> {
    let snap = load_snapshot(path);
    let mut c = pg.conn();

    // Replace, don't append: truncate all domain tables in one transaction so re-running is safe.
    let mut tx = c.transaction().map_err(|e| format!("import truncate tx: {e}"))?;
    tx.batch_execute(
        "TRUNCATE actors, accounts, repos, issues, prs, reviews, comments, session_records, \
         projects, users, teams, ai_connections, ai_rotate, ai_usage, owners",
    )
    .map_err(|e| format!("import truncate: {e}"))?;
    tx.commit().map_err(|e| format!("import truncate commit: {e}"))?;

    let mut stats = ImportStats::default();
    for actor in snap.actors.into_values() {
        pg.put_actor(actor);
        stats.actors += 1;
    }
    for account in snap.accounts.into_values() {
        pg.put_account(account);
        stats.accounts += 1;
    }
    for repo in snap.repos.into_values() {
        pg.put_repo(repo);
        stats.repos += 1;
    }
    for issue in snap.issues {
        pg.put_issue(issue);
        stats.issues += 1;
    }
    for pr in snap.prs {
        pg.put_pr(pr);
        stats.prs += 1;
    }
    for review in snap.reviews {
        pg.put_review(review);
        stats.reviews += 1;
    }
    for comment in snap.comments {
        pg.put_comment(comment);
        stats.comments += 1;
    }
    for session in snap.sessions {
        pg.put_session_record(session);
        stats.sessions += 1;
    }
    for project in snap.projects {
        pg.put_project(project);
        stats.projects += 1;
    }
    for user in snap.users.into_values() {
        pg.put_user(user);
        stats.users += 1;
    }
    for team in snap.teams.into_values() {
        pg.put_team(team);
        stats.teams += 1;
    }
    for conn in snap.ai_conns {
        pg.put_ai_connection(conn);
        stats.ai_connections += 1;
    }
    for (owner, on) in snap.ai_rotate {
        pg.set_ai_rotate(&owner, on);
        stats.ai_rotate += 1;
    }
    for (repo, rules) in snap.owners {
        pg.set_owners(&repo, rules);
        stats.owners += 1;
    }
    // Usage is an absolute tally, not an increment — set it directly rather than via add_ai_usage.
    for (conn_id, usage) in snap.ai_usage {
        c.execute(
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
        .map_err(|e| format!("import ai_usage: {e}"))?;
        stats.ai_usage += 1;
    }
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

    #[test]
    fn file_store_persists_across_reopen() {
        let dir = std::env::temp_dir().join(format!("hull-store-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("store.json");

        {
            let s = FileStore::open(&path);
            s.put_issue(issue("acme/web", 1, "first"));
            s.put_issue(issue("acme/web", 2, "second"));
        }
        // reopen — data must survive
        let s2 = FileStore::open(&path);
        let issues = s2.issues("acme/web");
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[1].title, "second");
        assert!(s2.issues("other/repo").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn close_reason_roundtrips_through_snapshot() {
        let dir = std::env::temp_dir().join(format!("hull-store-test2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("store.json");
        let mut it = issue("r", 1, "x");
        it.status = IssueStatus::Closed { reason: CloseReason::Completed };
        FileStore::open(&path).put_issue(it);
        let reopened = FileStore::open(&path).issues("r");
        assert!(matches!(reopened[0].status, IssueStatus::Closed { reason: CloseReason::Completed }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── PostgresStore behavioral tests ────────────────────────────────────────────────────────
    // These need a real Postgres, so they're gated behind `HULL_TEST_DATABASE_URL` and SKIP (print
    // + early-return) when it's unset — CI here has no Postgres. A follow-up wires testcontainers /
    // a CI service. They assert the SAME behaviors as the InMemory/FileStore tests, so the trait's
    // semantics are proven identical across backends once a DB is available.

    /// Connect to the test Postgres, run migrations, and TRUNCATE all domain tables so each test
    /// starts clean. `None` (with a skip message) when `HULL_TEST_DATABASE_URL` is unset.
    fn pg_test_store() -> Option<PostgresStore> {
        let url = std::env::var("HULL_TEST_DATABASE_URL").ok().filter(|u| !u.is_empty())?;
        let store = PostgresStore::connect(&url).expect("connect to HULL_TEST_DATABASE_URL");
        store
            .conn()
            .batch_execute(
                "TRUNCATE actors, accounts, repos, issues, prs, reviews, comments, session_records, \
                 projects, users, teams, ai_connections, ai_rotate, ai_usage, owners",
            )
            .expect("truncate test tables");
        Some(store)
    }

    macro_rules! pg_or_skip {
        ($name:literal) => {
            match pg_test_store() {
                Some(s) => s,
                None => {
                    eprintln!("skipping {}: HULL_TEST_DATABASE_URL unset (no Postgres)", $name);
                    return;
                }
            }
        };
    }

    #[test]
    fn pg_issue_crud_and_replace_and_update() {
        let s = pg_or_skip!("pg_issue_crud_and_replace_and_update");
        s.put_issue(issue("acme/web", 1, "first"));
        s.put_issue(issue("acme/web", 2, "second"));
        let issues = s.issues("acme/web");
        assert_eq!(issues.len(), 2);
        assert!(s.issues("other/repo").is_empty());

        // replace_issue: matches (repo, number), true iff one existed.
        let mut r = issue("acme/web", 2, "second-replaced");
        r.body = "b".into();
        assert!(s.replace_issue(r));
        assert!(!s.replace_issue(issue("acme/web", 99, "nope")));
        let after = s.issues("acme/web");
        assert_eq!(after.iter().find(|i| i.number == 2).unwrap().title, "second-replaced");

        // update_issue_content: None leaves a field unchanged; edited_unix is stamped.
        assert!(s.update_issue_content("acme/web", 1, Some("first-edited"), None, 42));
        assert!(!s.update_issue_content("acme/web", 999, Some("x"), None, 42));
        let i1 = s.issues("acme/web").into_iter().find(|i| i.number == 1).unwrap();
        assert_eq!(i1.title, "first-edited");
        assert_eq!(i1.body, ""); // body left unchanged (None)
        assert_eq!(i1.edited_unix, Some(42));
    }

    #[test]
    fn pg_purge_and_rekey_repo_data() {
        let s = pg_or_skip!("pg_purge_and_rekey_repo_data");
        s.put_issue(issue("acme/web", 1, "a"));
        s.put_issue(issue("acme/web", 2, "b"));
        s.put_issue(issue("acme/api", 1, "keep"));
        s.set_owners("acme/web", vec![OwnerRule { glob: "*.rs".into(), owners: vec!["you".into()] }]);

        // rekey moves both the key column and the `repo` field inside the stored object.
        s.rekey_repo_data("acme/web", "acme/web2");
        assert!(s.issues("acme/web").is_empty());
        let moved = s.issues("acme/web2");
        assert_eq!(moved.len(), 2);
        assert!(moved.iter().all(|i| i.repo == "acme/web2"));
        assert_eq!(s.owners("acme/web2").len(), 1);
        assert!(s.owners("acme/web").is_empty());
        assert_eq!(s.issues("acme/api").len(), 1); // untouched

        // purge removes only the target repo's rows.
        s.purge_repo_data("acme/web2");
        assert!(s.issues("acme/web2").is_empty());
        assert!(s.owners("acme/web2").is_empty());
        assert_eq!(s.issues("acme/api").len(), 1);
    }

    #[test]
    fn pg_session_upsert_and_ai_usage_increment() {
        let s = pg_or_skip!("pg_session_upsert_and_ai_usage_increment");
        // Latest write wins per (repo, change).
        let mut rec = SessionRecord { repo: "r".into(), change: "c1".into(), task: "t1".into(), model: String::new(), lesson: String::new(), tool_calls: 0, tokens_in: 0, tokens_out: 0 };
        s.put_session_record(rec.clone());
        rec.task = "t2".into();
        s.put_session_record(rec);
        assert_eq!(s.session_record("r", "c1").unwrap().task, "t2");
        assert!(s.session_record("r", "missing").is_none());

        // add_ai_usage accumulates atomically.
        s.add_ai_usage("conn-1", 10, 5, 100, 1000);
        s.add_ai_usage("conn-1", 3, 2, 50, 2000);
        let u = s.ai_usage("conn-1");
        assert_eq!(u.input_tokens, 13);
        assert_eq!(u.output_tokens, 7);
        assert_eq!(u.cost_micros, 150);
        assert_eq!(u.runs, 2);
        assert_eq!(u.updated_unix, 2000);
        assert_eq!(s.ai_usage("absent").runs, 0); // default zeros
    }

    #[test]
    fn pg_users_case_insensitive_lookup() {
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
        s.put_user(u);
        assert_eq!(s.user("u1").unwrap().username, "Justin");
        assert_eq!(s.user_by_username("justin").unwrap().id, "u1"); // case-insensitive
        assert_eq!(s.user_by_actor("actor-1").unwrap().id, "u1");
        assert!(s.user_by_username("nobody").is_none());
    }
}
