//! The OSS Hull server binary — the core, with no plugins beyond the built-ins.
//!
//! A hosted deployment does NOT edit this file: it lives in a separate private repo, depends on
//! this crate as a library, and calls `hull_server::run(opts, |reg| hull_hosted::register(reg))`.
//! See PLUGINS.md.

#[tokio::main]
async fn main() {
    // No extra plugins — just the core built-ins. The AI reviewer and other closed capabilities live
    // in the separate private hull-hosted repo, whose binary calls
    // `hull_server::run(opts, |reg| hull_hosted_plugins::register(reg))`. See PLUGINS.md.
    hull_server::run(hull_server::Options::default(), |_reg| {}).await;
}
