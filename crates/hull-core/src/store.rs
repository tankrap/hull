//! Storage seam. The scaffold ships an in-memory store; M1+ swaps in a SQL-backed implementation
//! (accounts/issues/projects as relational rows) that references keel objects by content address.
//! Keeping this a trait means the server, tests, and the eventual SQL store all share one shape.

use crate::{Account, Actor, Issue, Project, PullRequest, Repo};
use std::collections::HashMap;
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
