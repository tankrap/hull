-- Enforce one issue/PR number per repo at the database level.
--
-- The create path allocates a number as MAX(number)+1 (serialized in-process by the server's
-- allocation lock). This UNIQUE index is the durable backstop for that invariant: it guarantees the
-- number is unique even if two writers ever race, and it makes `replace_issue`/`replace_pr`
-- (UPDATE ... WHERE repo = $1 AND number = $2) address exactly one row. Previously the index was
-- non-unique, so a duplicate (repo, number) — had one been created — would have been UPDATE-d on ALL
-- matching rows under Postgres while the in-memory store touched only the first, a silent divergence.
--
-- Replaces the non-unique indexes created in 0001_init.sql, reusing the same names. On a database
-- that already holds a duplicate (repo, number) the CREATE UNIQUE fails and this migration's
-- transaction rolls back, surfacing the corruption rather than masking it.
DROP INDEX IF EXISTS issues_repo_number;
CREATE UNIQUE INDEX IF NOT EXISTS issues_repo_number ON issues (repo, number);
DROP INDEX IF EXISTS prs_repo_number;
CREATE UNIQUE INDEX IF NOT EXISTS prs_repo_number ON prs (repo, number);
