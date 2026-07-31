//! Secret scanning — one engine, two deployment points.
//!
//! - **Client-side, in the keel CLI**: a `keel scan` / pre-push guard runs this before a push, so a
//!   detected secret is stopped *before it ever leaves the machine* (the only place a leak is truly
//!   prevented rather than mitigated).
//! - **Server-side, in Hull**: `receive-pack` runs the identical engine as a backstop, so a push
//!   from a client without the guard is still caught. Same crate ⇒ guaranteed parity.
//!
//! Findings **redact** the matched secret (only a short fingerprint is kept), so scanning never
//! itself becomes a way to exfiltrate the value into logs.

use regex::Regex;
use serde::Serialize;

/// A detected secret. The literal value is never stored — only enough to locate and dedupe it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Finding {
    /// Stable rule id, e.g. `aws-access-key-id`.
    pub rule: String,
    /// Human label for the rule.
    pub title: String,
    /// 1-based line.
    pub line: usize,
    /// 1-based column where the match starts.
    pub column: usize,
    /// Redacted preview: first/last few chars, middle masked.
    pub redacted: String,
    /// Short non-reversible fingerprint (for dedupe / allow-listing without keeping the secret).
    pub fingerprint: String,
}

struct Rule {
    id: &'static str,
    title: &'static str,
    re: Regex,
    /// If set, the match is only a finding when the captured value's Shannon entropy (bits/char)
    /// exceeds this — cuts false positives on generic `key = "..."` style rules.
    min_entropy: Option<f64>,
    /// Which capture group holds the secret value (0 = whole match).
    group: usize,
}

/// The built-in ruleset. High-signal provider tokens plus an entropy-gated generic assignment rule.
fn rules() -> Vec<Rule> {
    let r = |id, title, pat: &str, group, min_entropy| Rule {
        id,
        title,
        re: Regex::new(pat).expect("valid pattern"),
        min_entropy,
        group,
    };
    vec![
        r("private-key", "Cryptographic private key", r"-----BEGIN (?:RSA |EC |OPENSSH |PGP |DSA )?PRIVATE KEY-----", 0, None),
        r("aws-access-key-id", "AWS access key id", r"\b(AKIA|ASIA)[0-9A-Z]{16}\b", 0, None),
        r("anthropic-key", "Anthropic API key", r"\bsk-ant-[A-Za-z0-9_-]{20,}\b", 0, None),
        r("openai-key", "OpenAI API key", r"\bsk-(?:proj-)?[A-Za-z0-9]{20,}\b", 0, None),
        r("openrouter-key", "OpenRouter API key", r"\bsk-or-v1-[A-Za-z0-9]{20,}\b", 0, None),
        r("github-token", "GitHub token", r"\b(ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36}\b", 0, None),
        r("gitlab-token", "GitLab token", r"\bglpat-[A-Za-z0-9_-]{20}\b", 0, None),
        r("slack-token", "Slack token", r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b", 0, None),
        r("google-api-key", "Google API key", r"\bAIza[0-9A-Za-z_-]{35}\b", 0, None),
        r("stripe-secret-key", "Stripe secret key", r"\b(sk|rk)_live_[A-Za-z0-9]{20,}\b", 0, None),
        r("jwt", "JSON Web Token", r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b", 0, None),
        // Generic: `SOMETHING_secret = "high-entropy-value"` — entropy-gated to avoid noise.
        r(
            "generic-secret-assignment",
            "High-entropy secret assignment",
            r#"(?i)(?:secret|token|passwd|password|api[_-]?key|access[_-]?key|private[_-]?key)[a-z0-9_]*\s*[:=]\s*['"]([A-Za-z0-9+/_=-]{20,})['"]"#,
            1,
            Some(3.5),
        ),
    ]
}

/// Scan `text` (the content of `path`, used only for context in callers) and return every secret
/// finding. Deterministic and side-effect free.
pub fn scan(text: &str) -> Vec<Finding> {
    let rules = rules();
    let mut out = Vec::new();
    for (li, line) in text.lines().enumerate() {
        for rule in &rules {
            for caps in rule.re.captures_iter(line) {
                let m = match caps.get(rule.group) {
                    Some(m) => m,
                    None => continue,
                };
                let value = m.as_str();
                if let Some(min) = rule.min_entropy {
                    if shannon_bits_per_char(value) < min {
                        continue;
                    }
                }
                out.push(Finding {
                    rule: rule.id.to_string(),
                    title: rule.title.to_string(),
                    line: li + 1,
                    column: m.start() + 1,
                    redacted: redact(value),
                    fingerprint: fingerprint(value),
                });
            }
        }
    }
    out
}

/// Whether `text` contains any secret (fast path for the pre-push guard's yes/no gate).
pub fn has_secret(text: &str) -> bool {
    !scan(text).is_empty()
}

/// Mask the middle of a secret, keeping a few edge chars so a human can recognize which key leaked
/// without the value being recoverable from the report.
fn redact(s: &str) -> String {
    let n = s.chars().count();
    if n <= 8 {
        return "*".repeat(n);
    }
    let head: String = s.chars().take(4).collect();
    let tail: String = s.chars().skip(n - 4).collect();
    format!("{head}…{tail} ({n} chars)")
}

/// A short, non-reversible fingerprint (FNV-1a → hex) for dedupe and allow-listing without storing
/// the secret. Not a security primitive — just a stable label.
fn fingerprint(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", h)
}

/// Shannon entropy in bits per character — used to gate the generic assignment rule.
fn shannon_bits_per_char(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut counts = [0u32; 256];
    let mut total = 0u32;
    for b in s.bytes() {
        counts[b as usize] += 1;
        total += 1;
    }
    let total = total as f64;
    -counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / total;
            p * p.log2()
        })
        .sum::<f64>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catches_high_signal_provider_secrets_and_redacts() {
        let text = "\
const cfg = {\n\
  aws: \"AKIAIOSFODNN7EXAMPLE\",\n\
  gh: \"ghp_012345678901234567890123456789abcdef\",\n\
};\n";
        let f = scan(text);
        let rules: Vec<_> = f.iter().map(|x| x.rule.as_str()).collect();
        assert!(rules.contains(&"aws-access-key-id"));
        assert!(rules.contains(&"github-token"));
        // the literal secret must never appear verbatim in a finding
        for finding in &f {
            assert!(!finding.redacted.contains("AKIAIOSFODNN7EXAMPLE"));
            assert!(finding.redacted.contains('…'));
        }
    }

    #[test]
    fn entropy_gate_rejects_obvious_placeholders() {
        // low-entropy placeholder should NOT trip the generic rule
        assert!(scan(r#"api_key = "xxxxxxxxxxxxxxxxxxxxxxxx""#).is_empty());
        // a real high-entropy value should
        assert!(has_secret(r#"api_key = "a8Fj2kLp9QzR4tVwX7yB1cD6eG0hN3mS""#));
    }

    #[test]
    fn clean_code_has_no_findings() {
        assert!(scan("fn main() {\n    println!(\"hello\");\n}\n").is_empty());
    }

    #[test]
    fn private_key_block_is_caught() {
        assert!(has_secret("-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END..."));
    }
}
