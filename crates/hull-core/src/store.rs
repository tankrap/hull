//! Storage seam. The scaffold ships an in-memory store; M1+ swaps in a SQL-backed implementation
//! (accounts/issues/projects as relational rows) that references keel objects by content address.
//! Keeping this a trait means the server, tests, and the eventual SQL store all share one shape.

use crate::{Account, Actor, Comment, Issue, OwnerRule, Project, PullRequest, Repo, Review, SessionRecord, Team, User};
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
    /// Associate an ingested keel session with a change (latest write wins per change).
    fn put_session_record(&self, record: SessionRecord);
    fn session_record(&self, repo: &str, change: &str) -> Option<SessionRecord>;
    fn put_project(&self, project: Project);
    fn projects(&self, owner: &str) -> Vec<Project>;
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

/// Match a code-owner glob against a repo-relative path. Supports `dir/**` (prefix), `*.ext`
/// (extension), and exact paths — enough for `.hull/owners`-style rules.
pub fn glob_match(glob: &str, path: &str) -> bool {
    if let Some(dir) = glob.strip_suffix("/**") {
        return path == dir || path.starts_with(&format!("{dir}/"));
    }
    if let Some(ext) = glob.strip_prefix("*.") {
        return path.ends_with(&format!(".{ext}"));
    }
    path == glob || path.starts_with(&format!("{glob}/"))
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
