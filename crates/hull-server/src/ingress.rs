//! Coordination **ingress** (NEW-1169, hosted topology): daemons dial INTO hull over QUIC and
//! stream their coordination events up, tenant-scoped. This is the inverse of the dev-only bridge
//! where hull dialed out to a local keeld — a hosted hull can't reach daemons behind NAT, so the
//! daemon side (`hull-agent`, running next to keeld) initiates the connection.
//!
//! Wire: the agent opens one uni stream and sends length-prefixed frames — a header
//! `{"tenant","repo"}` first, then each raw keeld event (verbatim JSON). Hull maps every event to
//! an [`ActivityEvent`] and publishes it under the connection's tenant.

use crate::activity::ActivityHub;
use crate::keeld::map_event;
use std::net::SocketAddr;
use std::sync::Arc;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

const MAX_FRAME: usize = 1 << 20; // 1 MiB — an event is small; reject anything larger

/// Start the ingress server on `addr` in the background. `expected_token` is the shared daemon token
/// required in each connection's header frame; `None` (the default, `HULL_INGRESS_TOKEN` unset) keeps
/// the ingress fully open — exactly as before this change.
pub fn spawn(addr: SocketAddr, hub: Arc<ActivityHub>, expected_token: Option<String>) {
    tokio::spawn(async move {
        if let Err(e) = serve(addr, hub, expected_token).await {
            eprintln!("hull: ingress server error: {e}");
        }
    });
}

async fn serve(addr: SocketAddr, hub: Arc<ActivityHub>, expected_token: Option<String>) -> Result<(), BoxError> {
    let endpoint = quinn::Endpoint::server(crate::quic::server_config()?, addr)?;
    eprintln!("hull: coordination ingress (QUIC) on {}", endpoint.local_addr()?);
    let expected_token = Arc::new(expected_token);
    while let Some(incoming) = endpoint.accept().await {
        let hub = hub.clone();
        let expected_token = expected_token.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(incoming, hub, expected_token.as_deref()).await {
                eprintln!("hull: ingress connection ended: {e}");
            }
        });
    }
    Ok(())
}

// AUTHORIZATION (authz-hardening, area D). This QUIC ingress supports an optional shared daemon token
// (`HULL_INGRESS_TOKEN`), threaded from config through `spawn`/`serve` into `handle_conn`. When SET,
// the first `{tenant,repo,token}` header frame must carry a matching `token` or the connection is
// rejected before any event is published. When UNSET (the default), the check is a no-op — the header
// `token` field is ignored and the current tokenless `hull-agent` uplink keeps working, so the wire
// stays backward-compatible. (A stronger per-tenant QUIC mTLS binding — see `quic.rs` — remains the
// roadmapped hardening for a multi-tenant hosted deploy; this token is the minimal shared-secret gate.)
async fn handle_conn(incoming: quinn::Incoming, hub: Arc<ActivityHub>, expected_token: Option<&str>) -> Result<(), BoxError> {
    let conn = incoming.await?;
    let peer = conn.remote_address();
    let mut recv = conn.accept_uni().await?;

    let header = read_frame(&mut recv).await?.ok_or("ingress: no header frame")?;
    let hdr: serde_json::Value = serde_json::from_slice(&header)?;
    let tenant = hdr.get("tenant").and_then(serde_json::Value::as_str).unwrap_or("local").to_string();
    let repo = hdr.get("repo").and_then(serde_json::Value::as_str).unwrap_or("").to_string();

    // When a daemon token is configured, the header frame must carry a matching `token` before any
    // event is published. Unset (the default) is a no-op — the header's `token` field is ignored and
    // the current tokenless daemon keeps working, so this stays backward-compatible with the wire.
    let presented = hdr.get("token").and_then(serde_json::Value::as_str);
    if !token_ok(expected_token, presented) {
        return Err(format!("ingress: rejected uplink for {tenant}/{repo} from {peer}: missing/invalid token").into());
    }
    eprintln!("hull: ingress uplink for {tenant}/{repo} from {peer}");

    while let Some(frame) = read_frame(&mut recv).await? {
        if let Some(ev) = map_event(&frame, &repo) {
            hub.publish(&tenant, ev);
        }
    }
    Ok(())
}

/// Whether an ingress connection's presented token satisfies the configured expectation. `None`
/// expected = ingress open to all (the default, `HULL_INGRESS_TOKEN` unset) — always OK. When a token
/// is expected, the header must carry exactly that value.
fn token_ok(expected: Option<&str>, presented: Option<&str>) -> bool {
    match expected {
        None => true,
        Some(exp) => presented == Some(exp),
    }
}

/// Read one length-prefixed frame (`u32` LE length + payload). `Ok(None)` when the stream ends.
async fn read_frame(recv: &mut quinn::RecvStream) -> Result<Option<Vec<u8>>, BoxError> {
    let mut len = [0u8; 4];
    if recv.read_exact(&mut len).await.is_err() {
        return Ok(None); // clean end of stream
    }
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err("ingress: frame exceeds size limit".into());
    }
    let mut buf = vec![0u8; n];
    recv.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

/// Frame a payload for the ingress wire (`u32` LE length + bytes). Used by `hull-agent`.
pub async fn write_frame(send: &mut quinn::SendStream, payload: &[u8]) -> Result<(), BoxError> {
    send.write_all(&(payload.len() as u32).to_le_bytes()).await?;
    send.write_all(payload).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::token_ok;

    #[test]
    fn ingress_token_gate() {
        // Unset expectation (default): every connection is accepted, with or without a token — the
        // backward-compatible no-op that keeps the current tokenless daemon working.
        assert!(token_ok(None, None), "no token configured → open (tokenless daemon)");
        assert!(token_ok(None, Some("anything")), "no token configured → a stray token is ignored");
        // Configured: only the exact token is accepted; missing or wrong is rejected.
        assert!(token_ok(Some("s3cret"), Some("s3cret")), "matching token accepted");
        assert!(!token_ok(Some("s3cret"), None), "missing token rejected when one is required");
        assert!(!token_ok(Some("s3cret"), Some("wrong")), "wrong token rejected");
    }
}
