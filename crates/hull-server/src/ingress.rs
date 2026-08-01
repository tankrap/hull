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

/// Start the ingress server on `addr` in the background.
pub fn spawn(addr: SocketAddr, hub: Arc<ActivityHub>) {
    tokio::spawn(async move {
        if let Err(e) = serve(addr, hub).await {
            eprintln!("hull: ingress server error: {e}");
        }
    });
}

async fn serve(addr: SocketAddr, hub: Arc<ActivityHub>) -> Result<(), BoxError> {
    let endpoint = quinn::Endpoint::server(crate::quic::server_config()?, addr)?;
    eprintln!("hull: coordination ingress (QUIC) on {}", endpoint.local_addr()?);
    while let Some(incoming) = endpoint.accept().await {
        let hub = hub.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(incoming, hub).await {
                eprintln!("hull: ingress connection ended: {e}");
            }
        });
    }
    Ok(())
}

async fn handle_conn(incoming: quinn::Incoming, hub: Arc<ActivityHub>) -> Result<(), BoxError> {
    let conn = incoming.await?;
    let peer = conn.remote_address();
    let mut recv = conn.accept_uni().await?;

    let header = read_frame(&mut recv).await?.ok_or("ingress: no header frame")?;
    let hdr: serde_json::Value = serde_json::from_slice(&header)?;
    let tenant = hdr.get("tenant").and_then(serde_json::Value::as_str).unwrap_or("local").to_string();
    let repo = hdr.get("repo").and_then(serde_json::Value::as_str).unwrap_or("").to_string();
    eprintln!("hull: ingress uplink for {tenant}/{repo} from {peer}");

    while let Some(frame) = read_frame(&mut recv).await? {
        if let Some(ev) = map_event(&frame, &repo) {
            hub.publish(&tenant, ev);
        }
    }
    Ok(())
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
