//! The OSS Hull server binary — the core, with no plugins beyond the built-ins.
//!
//! A hosted deployment does NOT edit this file: it lives in a separate private repo, depends on
//! this crate as a library, and calls `hull_server::run(opts, |reg| hull_hosted::register(reg))`.
//! See PLUGINS.md.

#[tokio::main]
async fn main() {
    // Deployment wiring (belongs in the hosted binary in the real split): activate the OpenRouter AI
    // reviewer *only* when a key resolves through the pluggable config — no key ⇒ the OSS
    // reconciliation reviewer stays the default, so the core is fully functional on its own.
    hull_server::run(hull_server::Options::default(), |reg| {
        if let Some(key) = reg.config("OPENROUTER_API_KEY") {
            // D4 model tiering: a cheap triage model screens; only escalations hit the deep model.
            let screen = reg.config("HULL_REVIEW_MODEL").unwrap_or_else(|| "anthropic/claude-sonnet-5".to_string());
            let deep = reg.config("HULL_REVIEW_MODEL_DEEP").unwrap_or_else(|| "anthropic/claude-opus-4.8".to_string());
            // D6 — provider allowlist (comma-separated vendors, e.g. "anthropic,openai"); empty = any.
            let allow: Vec<String> = reg
                .config("HULL_REVIEW_PROVIDER_ALLOWLIST")
                .map(|s| s.split(',').map(|v| v.trim().to_lowercase()).filter(|v| !v.is_empty()).collect())
                .unwrap_or_default();
            eprintln!("hull: OpenRouter AI reviewer active (triage {screen} → deep {deep}; allow={allow:?})");
            reg.set_reviewer(std::sync::Arc::new(hull_review_openrouter::OpenRouterReviewer::new(key, screen, deep, allow)));
        }
    })
    .await;
}
