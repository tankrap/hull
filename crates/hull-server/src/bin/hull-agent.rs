//! `hull-agent` — runs next to a local `keeld` and forwards its coordination stream UP to a hosted
//! hull's QUIC ingress. This is the daemon-side of the hosted topology: the agent dials out (so
//! hull needs no route back to a NAT'd machine), authenticating its tenant/repo.
//!
//! Usage:
//!   hull-agent --keeld 127.0.0.1:9000 --hull 127.0.0.1:8931 --tenant tankrap --repo hull
//!
//! Subscribes to the local keeld (keel-net), opens one uni stream to hull, sends a header, then
//! forwards every event verbatim. Reconnects with capped backoff.

use hull_server::ingress::write_frame;
use std::time::Duration;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let keeld = flag(&args, "--keeld").unwrap_or_else(|| "127.0.0.1:9000".into());
    let hull = flag(&args, "--hull").unwrap_or_else(|| "127.0.0.1:8931".into());
    let tenant = flag(&args, "--tenant").unwrap_or_else(|| "local".into());
    let repo = flag(&args, "--repo").unwrap_or_else(|| "repo".into());
    // Optional daemon token (matches the server's `HULL_INGRESS_TOKEN`): `--token` or env. Omitted →
    // no `token` field in the header, which the server accepts when it has no token configured.
    let token = flag(&args, "--token").or_else(|| std::env::var("HULL_INGRESS_TOKEN").ok()).filter(|t| !t.is_empty());

    eprintln!("hull-agent: {tenant}/{repo}  keeld={keeld}  hull={hull}");
    let mut backoff = 1u64;
    loop {
        match run(&keeld, &hull, &tenant, &repo, token.as_deref()).await {
            Ok(()) => backoff = 1,
            Err(e) => eprintln!("hull-agent: link down ({e})"),
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}

async fn run(keeld: &str, hull: &str, tenant: &str, repo: &str, token: Option<&str>) -> Result<(), BoxError> {
    // 1. subscribe to the local keeld's coordination stream
    let client = keel_net::Client::connect(keeld.parse()?).await?;
    let mut events = client.subscribe().await?;

    // 2. dial the hull ingress and open a uni stream
    let mut endpoint = quinn::Endpoint::client("[::]:0".parse()?)?;
    endpoint.set_default_client_config(hull_server::quic::client_config()?);
    let conn = endpoint.connect(hull.parse()?, "localhost")?.await?;
    let mut send = conn.open_uni().await?;

    // 3. header (with the daemon token when configured), then forward every event verbatim
    let mut header = serde_json::json!({ "tenant": tenant, "repo": repo });
    if let Some(tok) = token {
        header["token"] = serde_json::Value::String(tok.to_string());
    }
    write_frame(&mut send, &serde_json::to_vec(&header)?).await?;
    eprintln!("hull-agent: linked {tenant}/{repo} → {hull}");

    while let Some(bytes) = events.next().await {
        write_frame(&mut send, &bytes).await?;
    }
    let _ = send.finish();
    Ok(())
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}
