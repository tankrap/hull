//! Reconciliation engine — the substance of a Hull review (Epic C).
//!
//! An agent's change arrives with a *narrative*: the session's intent and lesson say what it claims
//! to have done ("added membership authz", "wrote tests", "no secrets left in"). Reconciliation
//! turns that prose into discrete **claims** and checks each one against the *facts* of the change —
//! the files/symbols actually touched, keel's verification signal, and the secret scan. Every claim
//! ends up **supported**, **contradicted**, or **unsupported**, each with the evidence that decided
//! it. This is what makes a Hull review more than a verdict: a line-by-line accounting of whether
//! the code does what its author said it does.
//!
//! Pure and deterministic — no I/O, no clock — so it is unit-testable and its output is
//! content-addressable alongside the change.

use serde::{Deserialize, Serialize};

/// The reconciled ledger for one change: every extracted claim with its status and evidence.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClaimLedger {
    pub change: String,
    pub claims: Vec<Claim>,
    /// C5 — **phantom work**: semantic operations the change actually made that **no claim accounts
    /// for**. The narrative didn't mention them, so a reviewer must — the agent did more (or other)
    /// than it said. Reconciliation checks claims→evidence; this is the reverse, evidence→claims.
    #[serde(default)]
    pub unclaimed: Vec<String>,
}

impl ClaimLedger {
    /// Count of claims the facts actively contradict — the number a reviewer must not ignore.
    pub fn contradicted(&self) -> usize {
        self.claims.iter().filter(|c| c.status == ClaimStatus::Contradicted).count()
    }
    /// Count with any corroborating evidence (mechanical / read-only / self-attested).
    pub fn supported(&self) -> usize {
        self.claims.iter().filter(|c| c.status.is_positive()).count()
    }
    /// Count that need a human's judgment (no evidence either way).
    pub fn needs_judgment(&self) -> usize {
        self.claims.iter().filter(|c| c.status == ClaimStatus::NeedsJudgment).count()
    }
    /// Count self-attested (green but the change tests itself) — a caution, not a verification.
    pub fn self_attested(&self) -> usize {
        self.claims.iter().filter(|c| c.status == ClaimStatus::SelfAttested).count()
    }
    /// Count of phantom-work operations — changes the narrative never claimed (C5). A reviewer must
    /// account for these: the change did more than it said.
    pub fn phantom(&self) -> usize {
        self.unclaimed.len()
    }
}

/// A single assertion extracted from the change's narrative.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Claim {
    /// Stable id: a hash of the normalized text, so the same claim reconciles to the same id.
    pub id: String,
    pub text: String,
    pub source: ClaimSource,
    pub status: ClaimStatus,
    pub evidence: Vec<Evidence>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClaimSource {
    /// From the change intent (`keel commit -m` / `Change.intent`).
    Intent,
    /// From the session's post-hoc lesson.
    Lesson,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClaimStatus {
    /// A mechanical check corroborates it — green keel verification (tests/CI) or a clean secret
    /// scan. The strongest positive available without running new probes.
    VerifiedMechanically,
    /// The reconciliation engine **read** the diff and found the claimed symbol/file/code. A real
    /// but weak positive — it confirms presence, not behavior. (`supported` is the pre-C4 alias.)
    #[serde(alias = "supported")]
    VerifiedReadOnly,
    /// Green, but the change **adds its own tests** — the passing tests may only cover this same
    /// change. Flagged, not independently verified.
    SelfAttested,
    /// Facts actively contradict it (claimed green tests but verification is red; claimed no
    /// secrets but the scan found one; plan says X, diff does Y).
    Contradicted,
    /// Nothing in the change speaks to it either way — a human must judge it. (`unsupported` alias.)
    #[serde(alias = "unsupported", other)]
    NeedsJudgment,
}

impl ClaimStatus {
    /// A positive (corroborated) status — any of the three "verified"/"attested" kinds.
    pub fn is_positive(&self) -> bool {
        matches!(self, ClaimStatus::VerifiedMechanically | ClaimStatus::VerifiedReadOnly | ClaimStatus::SelfAttested)
    }
}

/// The **independence-filtered** verification result: a change's checks re-run with any tests it
/// *added or modified* excluded (restored to their pre-change form, or dropped if newly added). This
/// is the honest answer to "does the change pass tests it did **not** author?" — a change must not be
/// allowed to approve itself by writing (or weakening) its own passing test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Independent {
    /// Pre-existing, unmodified tests pass against the new code. Genuine mechanical verification.
    Green,
    /// A test the change did **not** author fails against the new code — the change breaks a
    /// pre-existing test, or weakened/removed one that (restored) now fails. A hard contradiction.
    Red,
    /// No pre-existing test exercises the change — the only relevant tests are ones it added/modified.
    /// There is no independent signal, so a green run is self-attested, not verified.
    None,
}

/// Path-based heuristic for "this file is a test." Used to decide which files a change touched are
/// tests (so they can be discounted from the approval signal). Path-only — it cannot see inline unit
/// tests (`#[cfg(test)]` inside a source file); those are conservatively caught by [`ChangeFacts::adds_tests`].
pub fn is_test_path(path: &str) -> bool {
    let p = path.to_lowercase();
    // A `tests` / `test` / `__tests__` / `spec` path segment (anywhere, including the root), or a
    // conventional test-file name (`foo_test.rs`, `test_foo.py`, `x.test.ts`, `x.spec.ts`).
    p.split('/').any(|s| matches!(s, "tests" | "test" | "__tests__" | "spec"))
        || p.contains("test_")
        || p.contains("_test.")
        || p.contains(".test.")
        || p.contains(".spec.")
        || p.ends_with("_test.rs")
        || p.ends_with("_test.go")
}

/// A fact that bears on a claim, and whether it corroborates or undermines it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Evidence {
    /// `diff` | `verification` | `secret-scan`.
    pub kind: String,
    pub detail: String,
    pub supports: bool,
}

/// The observable facts of a change, gathered by the caller from the real diff / verification /
/// scan. Reconciliation reasons only over these — it never touches the repo itself.
#[derive(Debug, Clone, Default)]
pub struct ChangeFacts {
    /// Paths the change touched (added/modified/deleted).
    pub files: Vec<String>,
    /// Semantic operations detected in the diff, e.g. `added fn add_member`, `added state accounts`.
    pub ops: Vec<String>,
    /// keel verification: `green` | `red` | `unverified`.
    pub verification: String,
    /// Secret-scan findings on the change (rule titles); empty means clean.
    pub secrets: Vec<String>,
    /// The change's **added** lines, lower-cased and concatenated — the actual code, so a claim can be
    /// corroborated by what the diff literally introduced (an `onclick`, a css class, a string) even
    /// when no named function/type op captures it.
    pub added_text: String,
    /// Whether the change **adds test files** — so a green "tests pass" claim is self-attested (the
    /// change may only be tested by its own new tests) rather than mechanically verified. This is the
    /// coarse fallback used when [`independent_verification`](Self::independent_verification) is `None`.
    pub adds_tests: bool,
    /// The independence-filtered verification (tests the change added/modified excluded), when Hull
    /// computed it. `Some(_)` overrides the coarse `adds_tests` heuristic with a real signal;
    /// `None` means it wasn't computed (e.g. the change touched no tests, so the full suite is already
    /// independent) and reconciliation falls back to `verification` + `adds_tests`.
    pub independent_verification: Option<Independent>,
}

/// Reconcile a change's narrative against its facts. `intent` and `lesson` are the prose; `facts`
/// are what the change actually did.
pub fn reconcile(change: &str, intent: &str, lesson: &str, facts: &ChangeFacts) -> ClaimLedger {
    let mut claims = Vec::new();
    for text in split_claims(intent) {
        claims.push(judge(&text, ClaimSource::Intent, facts));
    }
    for text in split_claims(lesson) {
        // Don't double-count a lesson that merely restates the intent.
        if claims.iter().any(|c: &Claim| c.text.eq_ignore_ascii_case(&text)) {
            continue;
        }
        claims.push(judge(&text, ClaimSource::Lesson, facts));
    }
    // C5 — phantom work: any semantic op the change made that no claim's evidence cites. An op is
    // "claimed" when some claim matched it (its exact string appears as a `diff` evidence detail).
    let unclaimed: Vec<String> = facts
        .ops
        .iter()
        .filter(|op| !claims.iter().flat_map(|c| &c.evidence).any(|e| e.kind == "diff" && e.detail == **op))
        .cloned()
        .collect();
    ClaimLedger { change: change.to_string(), claims, unclaimed }
}

/// Reflow hard-wrapped prose into logical lines: a commit body wrapped at ~72 columns arrives as many
/// physical lines mid-sentence. Join a line onto the previous one unless it starts a new thought — a
/// blank line, a bullet/number marker, a trailer, or a previous line that already ended a statement
/// (`.`/`;`/`:`). Without this, a wrap boundary shears "…the issues/PRs" off "list now spans…".
fn reflow_lines(prose: &str) -> Vec<String> {
    let mut logical: Vec<String> = Vec::new();
    for raw in prose.lines() {
        let t = raw.trim();
        let bullet = t.starts_with("- ") || t.starts_with("* ") || t.chars().next().is_some_and(|c| c.is_ascii_digit());
        let starts_new = t.is_empty()
            || bullet
            || is_trailer(raw)
            || logical.last().is_none_or(|p| { let e = p.trim_end(); e.ends_with('.') || e.ends_with(';') || e.ends_with(':') });
        if starts_new {
            logical.push(raw.to_string());
        } else {
            let last = logical.last_mut().unwrap();
            last.push(' ');
            last.push_str(t);
        }
    }
    logical
}

/// Break a narrative into clause-level assertions. Splits on sentence and clause boundaries, drops
/// a leading conventional-commit prefix (`feat(x): ...`), and keeps only substantive clauses.
fn split_claims(prose: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw_line in reflow_lines(prose) {
        let raw_line = raw_line.as_str();
        if is_trailer(raw_line) {
            continue;
        }
        // Strip a conventional-commit prefix on the subject line only.
        let line = match raw_line.split_once("): ") {
            Some((head, rest)) if head.len() <= 24 && !head.contains(' ') => rest,
            _ => raw_line,
        };
        // Split on sentence/statement boundaries only — NOT commas. A comma usually joins parts of a
        // single assertion ("move X, directly above Y"); splitting on it shatters coherent prose into
        // fragments ("and enable", "directly above the list") that judge as noise.
        for clause in line.split(['.', ';', '\n']) {
            let c = clause.trim().trim_start_matches("- ").trim();
            // Keep clauses that read as an assertion. A real claim carries substance: either two
            // content words, or one long one (≥5 chars). This drops bare fragments like "the page"
            // (one short noun) and email shards while keeping "wrote tests" / "make it clickable".
            let sig = significant_words(&c.to_lowercase());
            let substantive = sig.len() >= 2 || sig.iter().any(|w| w.len() >= 5);
            if c.split_whitespace().count() >= 2 && substantive {
                out.push(c.to_string());
            }
        }
    }
    out
}

/// A git commit trailer (`Co-Authored-By: …`, `Signed-off-by: …`) or an email/URL shard — metadata,
/// not a claim about the change.
fn is_trailer(line: &str) -> bool {
    let l = line.trim();
    if l.contains('@') {
        return true;
    }
    match l.split_once(": ") {
        Some((key, _)) => {
            let k = key.trim();
            !k.is_empty()
                && !k.contains(' ')
                && k.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                && k.chars().all(|c| c.is_ascii_alphabetic() || c == '-')
                && k.contains('-')
        }
        None => false,
    }
}

/// Decide a single claim's status from the facts.
fn judge(text: &str, source: ClaimSource, facts: &ChangeFacts) -> Claim {
    let lower = text.to_lowercase();
    let mut evidence = Vec::new();

    // --- Verification claims (tests / CI / green) -------------------------------------------
    // Prefer the independence-filtered signal when Hull computed it: a change can't verify itself by
    // adding or weakening its own tests. Fall back to the plain verification when it wasn't computed.
    if mentions(&lower, &["test", "verif", "ci ", " ci", "green", "passes", "passing"]) {
        match facts.independent_verification {
            Some(Independent::Green) => evidence.push(Evidence {
                kind: "verification".into(),
                detail: "independent verification is green — tests the change did not author pass against the new code".into(),
                supports: true,
            }),
            Some(Independent::Red) => evidence.push(Evidence {
                kind: "verification".into(),
                detail: "independent verification is red — a pre-existing test the change did not author fails (it broke or weakened a test)".into(),
                supports: false,
            }),
            Some(Independent::None) => {
                // No pre-existing test exercises the change; only its own tests do. A green run here is
                // self-attested — record the green as weak support (the status engine downgrades it).
                if facts.verification == "green" {
                    evidence.push(Evidence {
                        kind: "verification".into(),
                        detail: "verification is green, but only the change's own tests exercise it (no independent coverage)".into(),
                        supports: true,
                    });
                } else if facts.verification == "red" {
                    evidence.push(Evidence { kind: "verification".into(), detail: "keel verification is red".into(), supports: false });
                }
            }
            None => match facts.verification.as_str() {
                "green" => evidence.push(Evidence {
                    kind: "verification".into(),
                    detail: "keel verification is green".into(),
                    supports: true,
                }),
                "red" => evidence.push(Evidence {
                    kind: "verification".into(),
                    detail: "keel verification is red — the change claims tests but they do not pass".into(),
                    supports: false,
                }),
                _ => {}
            },
        }
    }

    // --- Safety / secret claims -------------------------------------------------------------
    if mentions(&lower, &["secret", "no key", "no token", "credential", "safe", "redact"]) {
        if facts.secrets.is_empty() {
            evidence.push(Evidence {
                kind: "secret-scan".into(),
                detail: "secret scan is clean".into(),
                supports: true,
            });
        } else {
            evidence.push(Evidence {
                kind: "secret-scan".into(),
                detail: format!("secret scan flagged: {}", facts.secrets.join(", ")),
                supports: false,
            });
        }
    }

    // --- Symbol / file claims: does the diff touch what the claim names? --------------------
    // Match the claim's significant words against detected operations and touched paths.
    let words = significant_words(&lower);
    let mut matched_ops: Vec<&String> = facts
        .ops
        .iter()
        .filter(|op| {
            let opl = op.to_lowercase();
            words.iter().any(|w| word_in(&opl, w))
        })
        .collect();
    matched_ops.sort();
    matched_ops.dedup();
    for op in matched_ops.iter().take(3) {
        evidence.push(Evidence { kind: "diff".into(), detail: (*op).clone(), supports: true });
    }
    if matched_ops.is_empty() {
        let path_hit = facts.files.iter().find(|p| {
            let pl = p.to_lowercase();
            words.iter().any(|w| w.len() >= 4 && pl.contains(w.as_str()))
        });
        if let Some(p) = path_hit {
            evidence.push(Evidence {
                kind: "diff".into(),
                detail: format!("touches {p}"),
                supports: true,
            });
        } else if !facts.added_text.is_empty() {
            // Last resort: corroborate against the literal added code. A claim is supported if a
            // majority of its content words (min two) actually appear in what the diff introduced —
            // catches claims whose evidence is a call/attribute/string, not a named definition.
            let content: Vec<&String> = words.iter().filter(|w| w.len() >= 4).collect();
            let hits: Vec<&String> = content.iter().filter(|w| facts.added_text.contains(w.as_str())).copied().collect();
            if content.len() >= 2 && hits.len() * 2 >= content.len() {
                let sample: Vec<String> = hits.iter().take(3).map(|s| (*s).clone()).collect();
                evidence.push(Evidence {
                    kind: "diff".into(),
                    detail: format!("added code references {}", sample.join(", ")),
                    supports: true,
                });
            }
        }
    }

    // C4 status engine. Contradiction always wins. Otherwise rank the positive evidence: a mechanical
    // check (green verification / clean secret scan) beats a read-only diff match. A supporting
    // *verification* is mechanical only if it's independent of the change — otherwise self-attested.
    let status = if evidence.iter().any(|e| !e.supports) {
        ClaimStatus::Contradicted
    } else if evidence.iter().any(|e| e.supports && e.kind == "verification") {
        match facts.independent_verification {
            // Proven independent → mechanical, even if the change also adds tests of its own.
            Some(Independent::Green) => ClaimStatus::VerifiedMechanically,
            // Only the change's own tests cover it → self-attested regardless of `adds_tests`.
            Some(Independent::None) => ClaimStatus::SelfAttested,
            // Red is already handled by the contradiction branch above; unreachable here.
            Some(Independent::Red) => ClaimStatus::Contradicted,
            // Not computed → coarse fallback: adds-its-own-tests downgrades green to self-attested.
            None if facts.adds_tests => ClaimStatus::SelfAttested,
            None => ClaimStatus::VerifiedMechanically,
        }
    } else if evidence.iter().any(|e| e.supports && e.kind == "secret-scan") {
        ClaimStatus::VerifiedMechanically
    } else if evidence.iter().any(|e| e.supports) {
        ClaimStatus::VerifiedReadOnly
    } else {
        ClaimStatus::NeedsJudgment
    };

    Claim { id: claim_id(&lower), text: text.to_string(), source, status, evidence }
}

/// True if `haystack` contains any of the needles.
fn mentions(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

/// True if `word` appears in `haystack` on **word boundaries** — as a whole alphanumeric/underscore
/// token, not as a fragment of a larger word. Uses the same tokenizer as [`significant_words`], so a
/// claim word like "test" matches the op "wrote test" but NOT "latest". Raw `contains` would conflate
/// the two; word-boundary matching keeps a claim from being "supported" by an unrelated op that merely
/// spells its letters.
fn word_in(haystack: &str, word: &str) -> bool {
    haystack.split(|c: char| !c.is_alphanumeric() && c != '_').any(|tok| tok == word)
}

/// Content words worth matching against the diff — drops short/stop words.
fn significant_words(lower: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "the", "and", "for", "with", "that", "this", "into", "from", "only", "each", "over",
        "adds", "add", "added", "make", "makes", "made", "wire", "wired", "wires", "now", "use",
        "uses", "used", "via", "per", "its", "not", "are", "was", "when", "than", "must", "can",
        "new", "all", "any", "one", "gets", "get", "gains",
    ];
    lower
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| w.len() >= 3 && !STOP.contains(w))
        .map(|w| w.to_string())
        .collect()
}

/// Deterministic short id for a normalized claim (FNV-1a → hex), so identical claims share an id.
fn claim_id(normalized: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in normalized.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("clm_{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> ChangeFacts {
        ChangeFacts {
            files: vec!["crates/hull-server/src/lib.rs".into()],
            ops: vec!["added fn add_member".into(), "added fn accounts_list".into()],
            verification: "green".into(),
            secrets: vec![],
            added_text: String::new(),
            adds_tests: false,
            independent_verification: None,
        }
    }

    #[test]
    fn supported_symbol_claim() {
        let l = reconcile("c1", "feat(hull): add add_member handler", "", &facts());
        let claim = l.claims.iter().find(|c| c.text.contains("add_member")).unwrap();
        assert_eq!(claim.status, ClaimStatus::VerifiedReadOnly);
        assert!(claim.evidence.iter().any(|e| e.detail.contains("add_member")));
    }

    #[test]
    fn green_verification_supports_test_claim() {
        let l = reconcile("c1", "wrote tests for membership", "", &facts());
        let claim = l.claims.iter().find(|c| c.text.contains("tests")).unwrap();
        assert_eq!(claim.status, ClaimStatus::VerifiedMechanically);
    }

    #[test]
    fn green_but_change_adds_tests_is_self_attested() {
        let mut f = facts();
        f.adds_tests = true;
        let l = reconcile("c1", "tests pass", "", &f);
        let claim = l.claims.iter().find(|c| c.text.contains("tests")).unwrap();
        assert_eq!(claim.status, ClaimStatus::SelfAttested);
    }

    #[test]
    fn independent_green_is_mechanical_even_when_change_adds_tests() {
        // The change adds its own tests, but pre-existing tests (the ones it did NOT author) also
        // pass against the new code — that's genuine independent verification, not self-attestation.
        let mut f = facts();
        f.adds_tests = true;
        f.independent_verification = Some(Independent::Green);
        let l = reconcile("c1", "tests pass", "", &f);
        let claim = l.claims.iter().find(|c| c.text.contains("tests")).unwrap();
        assert_eq!(claim.status, ClaimStatus::VerifiedMechanically);
    }

    #[test]
    fn independent_none_is_self_attested_even_without_adds_tests_flag() {
        // Full suite is green, but no pre-existing test exercises the change — only its own do.
        let mut f = facts();
        f.adds_tests = false; // the coarse flag would say "mechanical"; the real signal says otherwise
        f.independent_verification = Some(Independent::None);
        let l = reconcile("c1", "tests pass", "", &f);
        let claim = l.claims.iter().find(|c| c.text.contains("tests")).unwrap();
        assert_eq!(claim.status, ClaimStatus::SelfAttested);
    }

    #[test]
    fn independent_red_contradicts_even_when_full_suite_is_green() {
        // The change weakened its own test so the full suite is green, but restoring the pre-existing
        // test (independence run) fails — the claim is contradicted, and the green can't hide it.
        let mut f = facts();
        f.verification = "green".into();
        f.independent_verification = Some(Independent::Red);
        let l = reconcile("c1", "all tests passing", "", &f);
        let claim = l.claims.iter().find(|c| c.text.contains("tests")).unwrap();
        assert_eq!(claim.status, ClaimStatus::Contradicted);
        assert_eq!(l.contradicted(), 1);
    }

    #[test]
    fn test_path_detection() {
        assert!(is_test_path("crates/foo/tests/integration.rs"));
        assert!(is_test_path("src/parser_test.rs"));
        assert!(is_test_path("web/src/App.test.ts"));
        assert!(is_test_path("api/handlers.spec.ts"));
        assert!(!is_test_path("crates/foo/src/lib.rs"));
        assert!(!is_test_path("README.md"));
    }

    #[test]
    fn red_verification_contradicts_test_claim() {
        let mut f = facts();
        f.verification = "red".into();
        let l = reconcile("c1", "all tests passing", "", &f);
        let claim = l.claims.iter().find(|c| c.text.contains("tests")).unwrap();
        assert_eq!(claim.status, ClaimStatus::Contradicted);
        assert_eq!(l.contradicted(), 1);
    }

    #[test]
    fn secret_hit_contradicts_safety_claim() {
        let mut f = facts();
        f.secrets = vec!["AWS access key".into()];
        let l = reconcile("c1", "no secrets committed", "", &f);
        let claim = l.claims.iter().find(|c| c.text.contains("secret")).unwrap();
        assert_eq!(claim.status, ClaimStatus::Contradicted);
    }

    #[test]
    fn unrelated_claim_is_unsupported() {
        let l = reconcile("c1", "refactored the pagination cursor", "", &facts());
        assert!(l.claims.iter().all(|c| c.status == ClaimStatus::NeedsJudgment));
    }

    #[test]
    fn added_code_corroborates_a_claim_with_no_named_op() {
        // "make it clickable" maps to no fn/type op, but the added code has an onClick.
        let mut f = facts();
        f.ops.clear();
        f.added_text = "const selectrepo = () => { onclick handler makes the repo clickable }".into();
        let l = reconcile("c1", "make the repo clickable", "", &f);
        let claim = l.claims.iter().find(|c| c.text.contains("clickable")).unwrap();
        assert_eq!(claim.status, ClaimStatus::VerifiedReadOnly);
        assert!(claim.evidence.iter().any(|e| e.detail.contains("added code references")));
    }

    #[test]
    fn bare_fragment_is_not_a_claim() {
        // "the page" is a sentence shard, not an assertion — it should not appear at all.
        let l = reconcile("c1", "reworked navigation. the page. added fn foo", "", &facts());
        assert!(!l.claims.iter().any(|c| c.text.eq_ignore_ascii_case("the page")));
    }

    #[test]
    fn hard_wrapped_prose_reflows_instead_of_shearing() {
        // A commit body wrapped mid-sentence must not shear "add the add_member" off "handler
        // and more" — the two physical lines are one assertion.
        let intent = "add the add_member\nhandler to the accounts module";
        let l = reconcile("c1", intent, "", &facts());
        assert!(l.claims.iter().any(|c| c.text == "add the add_member handler to the accounts module"), "wrapped lines should join: {:?}", l.claims.iter().map(|c| &c.text).collect::<Vec<_>>());
        // The shorn fragment "add the add_member" must NOT stand alone as its own claim.
        assert!(!l.claims.iter().any(|c| c.text == "add the add_member"));
    }

    #[test]
    fn phantom_work_is_flagged_when_the_narrative_omits_an_op() {
        // The intent claims add_member, but the change ALSO added accounts_list — unclaimed.
        let l = reconcile("c1", "add the add_member handler", "", &facts());
        assert!(l.claims.iter().any(|c| c.text.contains("add_member")));
        assert_eq!(l.unclaimed, vec!["added fn accounts_list".to_string()], "the unmentioned op is phantom work");
        assert_eq!(l.phantom(), 1);
    }

    #[test]
    fn nothing_is_phantom_when_every_op_is_claimed() {
        let l = reconcile("c1", "add add_member and accounts_list handlers", "", &facts());
        assert!(l.unclaimed.is_empty(), "both ops are named in the intent");
        assert_eq!(l.phantom(), 0);
    }

    #[test]
    fn claim_word_matches_op_on_word_boundary_not_substring() {
        // "test" must NOT be corroborated by an op that merely contains the letters (e.g. "latest").
        let mut f = facts();
        f.ops = vec!["added fn latest_snapshot".into()];
        f.verification = "unverified".into(); // don't let a verification claim carry it
        let l = reconcile("c1", "renamed the test helper", "", &f);
        let claim = l.claims.iter().find(|c| c.text.contains("test")).unwrap();
        // No op named "test"; only "latest_snapshot" — substring matching would wrongly support it.
        assert!(!claim.evidence.iter().any(|e| e.kind == "diff" && e.detail.contains("latest")),
            "the 'test' claim must not match the 'latest_snapshot' op: {:?}", claim.evidence);
    }

    #[test]
    fn claim_word_matches_whole_token_op() {
        // The word-boundary match still corroborates a claim whose word is a real token in the op.
        // (Avoid identifiers containing "test"/"verif"/etc. so this exercises the op-match path, not
        // the verification path.)
        let mut f = facts();
        f.ops = vec!["added fn snapshot_writer".into()];
        let l = reconcile("c1", "compute the snapshot_writer", "", &f);
        let claim = l.claims.iter().find(|c| c.text.contains("snapshot_writer")).unwrap();
        assert!(claim.evidence.iter().any(|e| e.kind == "diff" && e.detail.contains("snapshot_writer")));
        assert_eq!(claim.status, ClaimStatus::VerifiedReadOnly);
    }

    #[test]
    fn stable_id_across_runs() {
        let a = reconcile("c1", "add add_member handler", "", &facts());
        let b = reconcile("c2", "add add_member handler", "", &facts());
        assert_eq!(a.claims[0].id, b.claims[0].id);
    }
}
