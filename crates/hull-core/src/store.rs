//! Storage seam. The scaffold ships an in-memory store; M1+ swaps in a SQL-backed implementation
//! (accounts/issues/projects as relational rows) that references keel objects by content address.
//! Keeping this a trait means the server, tests, and the eventual SQL store all share one shape.

use crate::{Account, Actor, AiConnection, Comment, Issue, OwnerRule, Project, PullRequest, Repo, Review, SessionRecord, Team, User};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;

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
    fn put_issue(&self, issue: Issue);
    fn issues(&self, repo: &str) -> Vec<Issue>;
    /// Replace an existing issue, matched by `repo` + `number`. Returns true if one was replaced.
    fn replace_issue(&self, issue: Issue) -> bool;
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
        let inner = match std::fs::read(&path) {
            Err(_) => Snapshot::default(), // no file yet — a fresh store
            Ok(bytes) => match serde_json::from_slice::<Snapshot>(&bytes) {
                Ok(snap) => snap,
                Err(e) => {
                    let backup = path.with_extension(format!("json.corrupt-{}", std::process::id()));
                    let _ = std::fs::copy(&path, &backup);
                    panic!(
                        "hull: store at {} failed to parse ({e}); preserved a copy at {} and refused \
                         to start empty (which would overwrite your data). Migrate or restore, then restart.",
                        path.display(),
                        backup.display(),
                    );
                }
            },
        };
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
}
