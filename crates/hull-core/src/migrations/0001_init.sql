-- Hull domain store — initial schema (M1: JSON snapshot → Postgres).
--
-- Design: each entity is one row carrying its full serialized object in a JSONB `data` column,
-- with the fields the `Store` trait queries or matches on lifted out into indexed key columns.
-- This keeps the store an exact mirror of the in-memory/`Snapshot` model (byte-for-byte round-trip
-- of every field) while giving the trait's repo-scoped reads and keyed upserts real indexes.
--
-- Code, blobs, and provenance do NOT live here — those stay in the keel LMDB store and are
-- referenced by content address (see CodeRef.blob / PullRequest.changes / Issue.resolved_by).

-- Actors (human/agent identities), keyed by id.
CREATE TABLE actors (
    id   TEXT  PRIMARY KEY,
    data JSONB NOT NULL
);

-- Accounts (personal / organization), keyed by id.
CREATE TABLE accounts (
    id   TEXT  PRIMARY KEY,
    data JSONB NOT NULL
);

-- Repos, keyed by id. Unique on (owner, lower(name)) — one repo name per owner, case-insensitive.
CREATE TABLE repos (
    id    TEXT  PRIMARY KEY,
    owner TEXT  NOT NULL,
    name  TEXT  NOT NULL,
    data  JSONB NOT NULL
);
CREATE UNIQUE INDEX repos_owner_lower_name ON repos (owner, lower(name));
CREATE INDEX repos_owner ON repos (owner);

-- Issues. Append-on-put (numbers are unique in practice but not enforced, mirroring the in-memory
-- Vec). Matched by (repo, number) for replace/update; listed by repo.
CREATE TABLE issues (
    repo   TEXT   NOT NULL,
    number BIGINT NOT NULL,
    data   JSONB  NOT NULL
);
CREATE INDEX issues_repo_number ON issues (repo, number);
CREATE INDEX issues_repo ON issues (repo);

-- Pull requests. Same shape/semantics as issues.
CREATE TABLE prs (
    repo   TEXT   NOT NULL,
    number BIGINT NOT NULL,
    data   JSONB  NOT NULL
);
CREATE INDEX prs_repo_number ON prs (repo, number);
CREATE INDEX prs_repo ON prs (repo);

-- Reviews (first-class), listed by repo.
CREATE TABLE reviews (
    id   TEXT  NOT NULL,
    repo TEXT  NOT NULL,
    data JSONB NOT NULL
);
CREATE INDEX reviews_repo ON reviews (repo);

-- Comments, matched by (repo, id) for delete/update; listed by repo.
CREATE TABLE comments (
    id   TEXT  NOT NULL,
    repo TEXT  NOT NULL,
    data JSONB NOT NULL
);
CREATE INDEX comments_repo_id ON comments (repo, id);

-- Session records — latest write wins per (repo, change), so this is an upsert key.
CREATE TABLE session_records (
    repo   TEXT  NOT NULL,
    change TEXT  NOT NULL,
    data   JSONB NOT NULL
);
CREATE UNIQUE INDEX session_records_repo_change ON session_records (repo, change);

-- Projects, listed by owner.
CREATE TABLE projects (
    id    TEXT  NOT NULL,
    owner TEXT  NOT NULL,
    data  JSONB NOT NULL
);
CREATE INDEX projects_owner ON projects (owner);

-- Hosted-account login identities. Unique on lower(username); indexed by the actor they drive.
CREATE TABLE users (
    id       TEXT  PRIMARY KEY,
    username TEXT  NOT NULL,
    actor    TEXT  NOT NULL,
    data     JSONB NOT NULL
);
CREATE UNIQUE INDEX users_lower_username ON users (lower(username));
CREATE INDEX users_actor ON users (actor);

-- Org teams, keyed by id, listed by account.
CREATE TABLE teams (
    id      TEXT  PRIMARY KEY,
    account TEXT  NOT NULL,
    data    JSONB NOT NULL
);
CREATE INDEX teams_account ON teams (account);

-- AI connections (per owning account), listed by owner; matched by (owner, id) for removal.
CREATE TABLE ai_connections (
    id    TEXT  NOT NULL,
    owner TEXT  NOT NULL,
    data  JSONB NOT NULL
);
CREATE INDEX ai_connections_owner ON ai_connections (owner);

-- AI rotation flag per owner.
CREATE TABLE ai_rotate (
    owner   TEXT    PRIMARY KEY,
    on_flag BOOLEAN NOT NULL
);

-- AI usage tally per connection id — atomic increments (input=input+delta), not read-modify-write.
CREATE TABLE ai_usage (
    conn_id       TEXT   PRIMARY KEY,
    input_tokens  BIGINT NOT NULL DEFAULT 0,
    output_tokens BIGINT NOT NULL DEFAULT 0,
    cost_micros   BIGINT NOT NULL DEFAULT 0,
    runs          BIGINT NOT NULL DEFAULT 0,
    updated_unix  BIGINT NOT NULL DEFAULT 0
);

-- Code-owner rule sets, one row per repo (the whole Vec<OwnerRule> as a JSONB array).
CREATE TABLE owners (
    repo  TEXT  PRIMARY KEY,
    rules JSONB NOT NULL
);
