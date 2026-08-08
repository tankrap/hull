//! Minimal Blossom (BUD-01/02) client: mirror a repo's content-addressed blobs to public blob servers
//! so the actual file content, not just its refs and provenance, lives off this host. This is the last
//! rung of the "give-away-able substrate": relays carry refs (kind 31900) and provenance (kind 1900);
//! Blossom carries the blob bytes they point at.
//!
//! Blobs are addressed by sha256 (Blossom's key). Uploads carry a signed kind:24242 auth event (the
//! instance nostr key), base64'd into `Authorization: Nostr <event>`. On read the bytes are re-hashed
//! and checked against the requested sha256, so a blob server can never return content we didn't ask
//! for. Config-gated (off unless servers are set); best-effort, never on the request-critical path.

use base64::Engine;
use sha2::{Digest, Sha256};

pub struct BlossomClient {
    http: reqwest::Client,
    servers: Vec<String>, // base URLs, trailing slash trimmed
    secret_hex: String,   // instance nostr key, for BUD-01 auth events
}

impl BlossomClient {
    /// Build from env — `HULL_BLOSSOM_SERVERS` (comma/space-separated base URLs) + the shared
    /// `HULL_NOSTR_SECRET` for auth. `None` if unconfigured or the key is bad.
    pub fn from_env(http: reqwest::Client) -> Option<Self> {
        let secret_hex = std::env::var("HULL_NOSTR_SECRET").ok()?;
        crate::nostr::pubkey_of(&secret_hex)?; // validate the key
        let servers = Self::parse_servers(&std::env::var("HULL_BLOSSOM_SERVERS").ok()?);
        (!servers.is_empty()).then_some(Self { http, servers, secret_hex })
    }

    pub fn new(http: reqwest::Client, servers: Vec<String>, secret_hex: String) -> Self {
        Self { http, servers: Self::parse_servers(&servers.join(",")), secret_hex }
    }

    fn parse_servers(s: &str) -> Vec<String> {
        s.split([',', ' ']).filter(|s| !s.is_empty()).map(|s| s.trim_end_matches('/').to_string()).collect()
    }

    pub fn servers(&self) -> &[String] {
        &self.servers
    }

    /// The Blossom address of some bytes (lowercase hex sha256).
    pub fn sha256_hex(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    /// BUD-01 authorization: a signed kind:24242 event (verb + `x`=sha + short expiry), base64'd into
    /// an `Authorization: Nostr <base64(event)>` header value. `None` if the instance key is invalid.
    fn auth_header(&self, verb: &str, sha: &str) -> Option<String> {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let tags = vec![
            vec!["t".to_string(), verb.to_string()],
            vec!["x".to_string(), sha.to_string()],
            vec!["expiration".to_string(), (now + 300).to_string()],
        ];
        let ev = crate::nostr::build_event(&self.secret_hex, now, 24242, tags, &format!("{verb} blob"))?;
        Some(format!("Nostr {}", base64::engine::general_purpose::STANDARD.encode(ev.to_json().to_string())))
    }

    /// Upload `bytes` to the first server that accepts it. Returns the sha256 address on success, or
    /// `None` if every server refused / errored. Best-effort — the caller ignores the result.
    pub async fn upload(&self, bytes: Vec<u8>) -> Option<String> {
        let sha = Self::sha256_hex(&bytes);
        let auth = self.auth_header("upload", &sha)?;
        for base in &self.servers {
            let url = format!("{base}/upload");
            match self.http.put(&url).header("Authorization", &auth).body(bytes.clone()).send().await {
                Ok(r) if r.status().is_success() => return Some(sha),
                Ok(r) => eprintln!("blossom: {url} rejected upload: {}", r.status()),
                Err(e) => eprintln!("blossom: {url} upload error: {e}"),
            }
        }
        None
    }

    /// Fetch a blob by sha256 from the first server that has it. The returned bytes are re-hashed and
    /// MUST match `sha` — a blob server is never trusted to return the right content (content-address
    /// integrity). `None` if no server has it or every candidate failed the hash check.
    pub async fn get(&self, sha: &str) -> Option<Vec<u8>> {
        // Cap the body so a hostile server can't OOM us by streaming gigabytes before the hash check.
        const MAX_BLOB: u64 = 64 * 1024 * 1024;
        let auth = self.auth_header("get", sha);
        for base in &self.servers {
            let url = format!("{base}/{sha}");
            let mut req = self.http.get(&url);
            if let Some(a) = &auth {
                req = req.header("Authorization", a);
            }
            if let Ok(r) = req.send().await {
                if !r.status().is_success() {
                    continue;
                }
                // A declared oversize body is refused up front; a lying/omitted length is still bounded
                // below after the read (buffered, but reqwest's request timeout caps how much arrives).
                if r.content_length().is_some_and(|n| n > MAX_BLOB) {
                    eprintln!("blossom: {url} body too large ({:?}) — skipped", r.content_length());
                    continue;
                }
                if let Ok(b) = r.bytes().await {
                    if b.len() as u64 > MAX_BLOB {
                        eprintln!("blossom: {url} body exceeded {MAX_BLOB} bytes — rejected");
                        continue;
                    }
                    let bytes = b.to_vec();
                    if Self::sha256_hex(&bytes) == sha {
                        return Some(bytes);
                    }
                    eprintln!("blossom: {url} returned bytes that don't match {sha} — rejected");
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Path, State};
    use axum::routing::{get, put};
    use std::sync::{Arc, Mutex};

    const SK: &str = "0000000000000000000000000000000000000000000000000000000000000001";

    // A minimal in-process Blossom server: PUT /upload stores by sha256, GET /:hash returns it. Enough
    // to prove the upload → get → hash-verify round trip against a real HTTP endpoint.
    async fn spawn_blossom() -> String {
        type Store = Arc<Mutex<std::collections::HashMap<String, Vec<u8>>>>;
        let store: Store = Arc::new(Mutex::new(std::collections::HashMap::new()));
        async fn up(State(s): State<Store>, headers: axum::http::HeaderMap, body: axum::body::Bytes) -> Result<String, axum::http::StatusCode> {
            // Validate the BUD-01 auth: `Authorization: Nostr <base64(kind:24242 event)>`, verified,
            // with t=upload and x=<sha of the body>. This asserts the client's auth_header is well-formed.
            let unauth = axum::http::StatusCode::UNAUTHORIZED;
            let hdr = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
            let b64 = hdr.strip_prefix("Nostr ").ok_or(unauth)?;
            let json = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
                .ok_or(unauth)?;
            let ev = serde_json::from_str::<serde_json::Value>(&json).ok().as_ref().and_then(crate::nostr::Event::from_json).ok_or(unauth)?;
            let sha = BlossomClient::sha256_hex(&body);
            let ok = ev.verify()
                && ev.kind == 24242
                && ev.tags.iter().any(|t| t.len() == 2 && t[0] == "t" && t[1] == "upload")
                && ev.tags.iter().any(|t| t.len() == 2 && t[0] == "x" && t[1] == sha);
            if !ok {
                return Err(unauth);
            }
            s.lock().unwrap().insert(sha.clone(), body.to_vec());
            Ok(sha)
        }
        async fn dl(State(s): State<Store>, Path(hash): Path<String>) -> Result<Vec<u8>, axum::http::StatusCode> {
            s.lock().unwrap().get(&hash).cloned().ok_or(axum::http::StatusCode::NOT_FOUND)
        }
        let app = axum::Router::new().route("/upload", put(up)).route("/:hash", get(dl)).with_state(store);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    #[tokio::test]
    async fn upload_then_get_round_trips_and_verifies_the_hash() {
        let url = spawn_blossom().await;
        let c = BlossomClient::new(reqwest::Client::new(), vec![url], SK.into());
        let bytes = b"the actual file content".to_vec();
        let expect = BlossomClient::sha256_hex(&bytes);
        let sha = c.upload(bytes.clone()).await.expect("upload");
        assert_eq!(sha, expect, "upload returns the sha256 address");
        assert_eq!(c.get(&sha).await.as_deref(), Some(bytes.as_slice()), "the blob reads back");
        // an unknown hash yields nothing (server 404s)
        assert_eq!(c.get(&"0".repeat(64)).await, None);
    }

    #[tokio::test]
    async fn get_rejects_bytes_that_dont_match_the_requested_hash() {
        // A dishonest server that returns wrong bytes for any hash must be caught by the client's
        // re-hash check (content-address integrity — never trust the server's content).
        async fn liar(Path(_h): Path<String>) -> Vec<u8> {
            b"not what you asked for".to_vec()
        }
        let app = axum::Router::new().route("/:hash", get(liar));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let c = BlossomClient::new(reqwest::Client::new(), vec![url], SK.into());
        assert_eq!(c.get(&"a".repeat(64)).await, None, "mismatched bytes are rejected, not returned");
    }
}
