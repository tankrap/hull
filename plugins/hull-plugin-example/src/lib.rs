//! Reference plugin. A **closed hosted plugin is written exactly like this** — it depends only on
//! `hull-plugin` (the Apache-2.0 SDK), implements capability traits, and exposes a `register` entry
//! point the server calls behind a build feature. The only differences for a private plugin: the
//! crate lives in a separate private repo, and the server enables it via the `hosted` feature
//! instead of `example-plugins`. See `PLUGINS.md`.

use hull_plugin::{NotifyEvent, Notifier, Plugin, Registry, SecretRuleset};
use hull_scan::Finding;
use std::sync::Arc;

/// The plugin object the server installs.
pub struct ExamplePlugin;

impl Plugin for ExamplePlugin {
    fn name(&self) -> &str {
        "example"
    }
    fn description(&self) -> &str {
        "reference plugin: a stdout notifier + one extra secret rule"
    }
    fn register(&self, reg: &mut Registry) {
        reg.add_notifier(Arc::new(StdoutNotifier));
        reg.add_secret_ruleset(Arc::new(ExtraRules));
    }
}

/// A trivial notifier — prints to stdout. (Hosted's would deliver over email/Slack/nostr.)
struct StdoutNotifier;
impl Notifier for StdoutNotifier {
    fn notify(&self, event: &NotifyEvent) {
        println!("[notify:{}] -> {:?}: {}", event.kind, event.to, event.summary);
    }
}

/// One extra secret rule the built-in engine doesn't have — demonstrating a managed ruleset feed.
struct ExtraRules;
impl SecretRuleset for ExtraRules {
    fn extra_findings(&self, text: &str) -> Vec<Finding> {
        let mut out = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if let Some(col) = line.find("hull_pat_") {
                out.push(Finding {
                    rule: "example-hull-personal-token".into(),
                    title: "Hull personal access token (example rule)".into(),
                    line: i + 1,
                    column: col + 1,
                    redacted: "hull_pat_…".into(),
                    fingerprint: format!("example-{i}-{col}"),
                });
            }
        }
        out
    }
}

/// The registration entry point the server calls (feature-gated). A private hosted crate exposes
/// the identical function.
pub fn register(reg: &mut Registry) {
    reg.install(&ExamplePlugin);
}
