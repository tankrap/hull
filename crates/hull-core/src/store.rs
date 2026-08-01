//! Storage seam. The scaffold ships an in-memory store; M1+ swaps in a SQL-backed implementation
//! (accounts/issues/projects as relational rows) that references keel objects by content address.
//! Keeping this a trait means the server, tests, and the eventual SQL store all share one shape.

use crate::{Account, Actor, Issue, Project, PullRequest, Repo};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;

/// The persistence operations the server needs. Deliberately small for the scaffold — it grows per
/// milestone. All reads clone (cheap at this stage; the SQL store will stream).
pub trait Store: Send + Sync {
    fn put_actor(&self, actor: Actor);
    fn actor(&self, id: &str) -> Option<Actor>;
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
    fn put_project(&self, project: Project);
    fn projects(&self, owner: &str) -> Vec<Project>;
}

/// A thread-safe in-memory [`Store`] for the scaffold and tests.
#[derive(Default)]
pub struct InMemory {
    actors: RwLock<HashMap<String, Actor>>,
    accounts: RwLock<HashMap<String, Account>>,
    repos: RwLock<HashMap<String, Repo>>,
    issues: RwLock<Vec<Issue>>,
    prs: RwLock<Vec<PullRequest>>,
    projects: RwLock<Vec<Project>>,
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
    fn put_project(&self, project: Project) {
        self.projects.write().unwrap().push(project);
    }
    fn projects(&self, owner: &str) -> Vec<Project> {
        self.projects.read().unwrap().iter().filter(|p| p.owner == owner).cloned().collect()
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
    projects: Vec<Project>,
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
    /// Open the store at `path`, loading an existing snapshot or starting empty.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let inner = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Snapshot>(&b).ok())
            .unwrap_or_default();
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
    fn put_project(&self, project: Project) {
        self.mutate(|s| s.projects.push(project));
    }
    fn projects(&self, owner: &str) -> Vec<Project> {
        self.inner.read().unwrap().projects.iter().filter(|p| p.owner == owner).cloned().collect()
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
