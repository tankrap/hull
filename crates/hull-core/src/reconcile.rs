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
    /// but weak positive — it confirms presence, not behavior.
    VerifiedReadOnly,
    /// Green, but the change **adds its own tests** — the passing tests may only cover this same
    /// change. Flagged, not independently verified.
    SelfAttested,
    /// Facts actively contradict it (claimed green tests but verification is red; claimed no
    /// secrets but the scan found one; plan says X, diff does Y).
    Contradicted,
    /// Nothing in the change speaks to it either way — a human must judge it.
    NeedsJudgment,
}

impl ClaimStatus {
    /// A positive (corroborated) status — any of the three "verified"/"attested" kinds.
    pub fn is_positive(&self) -> bool {
        matches!(self, ClaimStatus::VerifiedMechanically | ClaimStatus::VerifiedReadOnly | ClaimStatus::SelfAttested)
    }
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
    /// change may only be tested by its own new tests) rather than mechanically verified.
    pub adds_tests: bool,
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
    ClaimLedger { change: change.to_string(), claims }
}

/// Break a narrative into clause-level assertions. Splits on sentence and clause boundaries, drops
/// a leading conventional-commit prefix (`feat(x): ...`), and keeps only substantive clauses.
fn split_claims(prose: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw_line in prose.lines() {
        if is_trailer(raw_line) {
            continue;
        }
        // Strip a conventional-commit prefix on the subject line only.
        let line = match raw_line.split_once("): ") {
            Some((head, rest)) if head.len() <= 24 && !head.contains(' ') => rest,
            _ => raw_line,
        };
        for clause in line.split(['.', ';', '\n', ',']) {
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
    if mentions(&lower, &["test", "verif", "ci ", " ci", "green", "passes", "passing"]) {
        match facts.verification.as_str() {
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
            words.iter().any(|w| opl.contains(w.as_str()))
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
    // check (green verification / clean secret scan) beats a read-only diff match; a mechanical
    // *verification* on a change that adds its own tests is only self-attested.
    let status = if evidence.iter().any(|e| !e.supports) {
        ClaimStatus::Contradicted
    } else if evidence.iter().any(|e| e.supports && e.kind == "verification") {
        if facts.adds_tests {
            ClaimStatus::SelfAttested
        } else {
            ClaimStatus::VerifiedMechanically
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
    fn stable_id_across_runs() {
        let a = reconcile("c1", "add add_member handler", "", &facts());
        let b = reconcile("c2", "add add_member handler", "", &facts());
        assert_eq!(a.claims[0].id, b.claims[0].id);
    }
}
