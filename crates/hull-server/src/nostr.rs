//! Minimal **nostr** (NIP-01) publisher for code-owner notifications.
//!
//! nostr is decentralized by design — signed events over public relays, no proprietary infra — so
//! this lives in the OSS core, not a hosted plugin: a self-hoster gets real fan-out for free. When an
//! actor opts in with a nostr pubkey ([`hull_core::Actor::nostr_pubkey`]) and code they own is
//! touched, Hull publishes a signed kind:1 note `p`-tagging them, carrying the keel change id so an
//! **agent** owner can act on it autonomously.
//!
//! The event id + BIP340 schnorr signature are the correctness-critical, unit-tested core; relay
//! delivery is best-effort I/O.

use hull_core::store::Store;
use hull_plugin::{NotifyEvent, Notifier};
use secp256k1::{Keypair, Message, Secp256k1, XOnlyPublicKey};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// A signed nostr event (NIP-01).
#[derive(Debug, Clone)]
pub struct Event {
    pub id: String,
    pub pubkey: String,
    pub created_at: u64,
    pub kind: u16,
    pub tags: Vec<Vec<String>>,
    pub content: String,
    pub sig: String,
}

/// The x-only nostr public key (hex) for a 32-byte secret (hex), or `None` if the secret is invalid.
pub fn pubkey_of(secret_hex: &str) -> Option<String> {
    let bytes = hex::decode(secret_hex.trim()).ok()?;
    let secp = Secp256k1::new();
    let kp = Keypair::from_seckey_slice(&secp, &bytes).ok()?;
    Some(hex::encode(kp.x_only_public_key().0.serialize()))
}

/// The NIP-01 canonical serialization whose SHA-256 is the event id:
/// `[0, pubkey, created_at, kind, tags, content]` as compact JSON.
fn serialize_for_id(pubkey: &str, created_at: u64, kind: u16, tags: &[Vec<String>], content: &str) -> String {
    let v = serde_json::json!([0, pubkey, created_at, kind, tags, content]);
    serde_json::to_string(&v).unwrap_or_default()
}

/// Build and sign a nostr event with `secret_hex` (the publisher's key). Deterministic signature
/// (no aux randomness) so the same event is reproducible. `None` if the secret is invalid.
pub fn build_event(secret_hex: &str, created_at: u64, kind: u16, tags: Vec<Vec<String>>, content: &str) -> Option<Event> {
    let bytes = hex::decode(secret_hex.trim()).ok()?;
    let secp = Secp256k1::new();
    let kp = Keypair::from_seckey_slice(&secp, &bytes).ok()?;
    let pubkey = hex::encode(kp.x_only_public_key().0.serialize());
    let id_bytes: [u8; 32] = Sha256::digest(serialize_for_id(&pubkey, created_at, kind, &tags, content).as_bytes()).into();
    let sig = secp.sign_schnorr_no_aux_rand(&Message::from_digest(id_bytes), &kp);
    Some(Event {
        id: hex::encode(id_bytes),
        pubkey,
        created_at,
        kind,
        tags,
        content: content.to_string(),
        sig: hex::encode(sig.serialize()),
    })
}

impl Event {
    /// The event as its NIP-01 JSON object.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id, "pubkey": self.pubkey, "created_at": self.created_at,
            "kind": self.kind, "tags": self.tags, "content": self.content, "sig": self.sig,
        })
    }
    /// The `["EVENT", <event>]` frame a relay accepts.
    pub fn relay_frame(&self) -> String {
        serde_json::json!(["EVENT", self.to_json()]).to_string()
    }
    /// Verify the event's own id and schnorr signature (self-consistency check).
    pub fn verify(&self) -> bool {
        let recomputed: [u8; 32] = Sha256::digest(serialize_for_id(&self.pubkey, self.created_at, self.kind, &self.tags, &self.content).as_bytes()).into();
        if hex::encode(recomputed) != self.id {
            return false;
        }
        let secp = Secp256k1::verification_only();
        let (Ok(pk), Ok(sig_bytes)) = (hex::decode(&self.pubkey), hex::decode(&self.sig)) else { return false };
        let (Ok(xonly), Ok(sig)) = (XOnlyPublicKey::from_slice(&pk), secp256k1::schnorr::Signature::from_slice(&sig_bytes)) else { return false };
        secp.verify_schnorr(&sig, &Message::from_digest(recomputed), &xonly).is_ok()
    }
}

/// Set read+write timeouts on a websocket's underlying TCP socket, for BOTH plain `ws://` and TLS
/// `wss://` — so a relay that accepts the connection but never replies (no EOSE) can't hang the
/// caller. tungstenite only exposes the socket per-variant; the plain path alone (as before) would
/// leave `wss` reads blocking forever.
fn set_ws_timeouts(ws: &tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>, d: std::time::Duration) {
    use tungstenite::stream::MaybeTlsStream;
    let sock: Option<&std::net::TcpStream> = match ws.get_ref() {
        MaybeTlsStream::Plain(s) => Some(s),
        MaybeTlsStream::Rustls(s) => Some(s.get_ref()),
        _ => None,
    };
    if let Some(s) = sock {
        let _ = s.set_read_timeout(Some(d));
        let _ = s.set_write_timeout(Some(d));
    }
}

/// Publish `event` to each relay (wss/ws url) best-effort; returns how many accepted the connection.
/// Bounded per-relay by a short timeout so a dead relay can't stall the caller. Fire-and-forget: the
/// notification path never fails because a relay is down.
pub fn publish(relays: &[String], event: &Event) -> usize {
    use std::time::Duration;
    let frame = event.relay_frame();
    let mut sent = 0;
    for url in relays {
        match tungstenite::connect(url) {
            Ok((mut ws, _resp)) => {
                set_ws_timeouts(&ws, Duration::from_secs(4));
                if ws.send(tungstenite::Message::Text(frame.clone())).is_ok() {
                    sent += 1;
                }
                let _ = ws.close(None);
            }
            Err(e) => eprintln!("nostr: relay {url} unreachable: {e}"),
        }
    }
    sent
}

// ── ref transport: publish a repo's branch pointer as a signed event, and read it back ────────────
//
// This is the decentralization substrate's first rung: a repo's refs live on public relays as signed
// events, so history isn't hostage to one host (the design's "give-away-able substrate"). Refs are
// **parameterized-replaceable** events (kind 31900): relays keep only the newest per (author, kind,
// `d`-tag), so `repo#branch` resolves to the latest commit. Signed by the INSTANCE key (transport
// authenticity); the Ed25519 keel-provenance bundle is a later layer.

/// Parameterized-replaceable ref event kind (NIP-01 30000–39999: latest-wins per `d`-tag).
pub const KIND_REF: u16 = 31900;

/// Build a signed ref event: `repo`'s `branch` now points at `commit` (with optional `prev`). The
/// `d`-tag `repo#branch` is what relays key the replaceable on. `None` if the secret is invalid.
pub fn ref_event(secret_hex: &str, repo: &str, branch: &str, commit: &str, prev: Option<&str>, created_at: u64) -> Option<Event> {
    let mut tags = vec![
        vec!["d".to_string(), format!("{repo}#{branch}")],
        vec!["repo".to_string(), repo.to_string()],
        vec!["ref".to_string(), branch.to_string()],
    ];
    if let Some(p) = prev {
        tags.push(vec!["prev".to_string(), p.to_string()]);
    }
    let content = serde_json::json!({ "commit": commit, "prev": prev, "repo": repo, "ref": branch }).to_string();
    build_event(secret_hex, created_at, KIND_REF, tags, &content)
}

impl Event {
    /// Parse a relay's event JSON object into an [`Event`] (the inverse of [`to_json`](Self::to_json)).
    pub fn from_json(v: &serde_json::Value) -> Option<Event> {
        Some(Event {
            id: v.get("id")?.as_str()?.to_string(),
            pubkey: v.get("pubkey")?.as_str()?.to_string(),
            created_at: v.get("created_at")?.as_u64()?,
            kind: v.get("kind")?.as_u64()? as u16,
            tags: serde_json::from_value(v.get("tags")?.clone()).ok()?,
            content: v.get("content")?.as_str()?.to_string(),
            sig: v.get("sig")?.as_str()?.to_string(),
        })
    }
}

/// The newest event's `commit` (parameterized-replaceable = latest-wins by `created_at`) — so a set of
/// ref events for the same `repo#branch` collapses to the current commit. Pure; the client resolves
/// latest-wins itself rather than trusting a single relay's collapse. Ties on `created_at` break by
/// lowest event id (NIP-01), so client and relay agree deterministically on the winner.
pub fn newest_commit(events: &[Event]) -> Option<String> {
    events
        .iter()
        .max_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| b.id.cmp(&a.id)))
        .and_then(|e| serde_json::from_str::<serde_json::Value>(&e.content).ok())
        .and_then(|c| c.get("commit").and_then(|x| x.as_str()).map(str::to_string))
}

/// Overall wall-clock budget for a [`fetch_events`] read across all relays, and a hard cap on events
/// collected — so a chatty/hostile relay that streams events without ever sending `EOSE` (which keeps
/// the per-read idle timeout from firing) can't hang the caller or grow memory without bound.
const FETCH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(6);
const FETCH_MAX_EVENTS: usize = 512;

/// Subscribe (NIP-01 `REQ`) for events matching `filter` across `relays`, collecting until `EOSE` or a
/// short timeout, returning every VERIFIED event (deduped by id). This is the read half the notifier
/// never needed — it's what makes refs on relays actually readable back.
pub fn fetch_events(relays: &[String], filter: serde_json::Value) -> Vec<Event> {
    use std::time::Duration;
    let req = serde_json::json!(["REQ", "hull-ref", filter]).to_string();
    let mut out: Vec<Event> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let start = std::time::Instant::now();
    for url in relays {
        if start.elapsed() > FETCH_DEADLINE || out.len() >= FETCH_MAX_EVENTS {
            break;
        }
        let Ok((mut ws, _)) = tungstenite::connect(url) else { continue };
        set_ws_timeouts(&ws, Duration::from_secs(4));
        if ws.send(tungstenite::Message::Text(req.clone())).is_err() {
            let _ = ws.close(None);
            continue;
        }
        loop {
            // Overall budget + cap: a relay that streams events without ever sending EOSE keeps the
            // per-read idle timeout from firing, so bound the loop by wall-clock and event count too.
            if start.elapsed() > FETCH_DEADLINE || out.len() >= FETCH_MAX_EVENTS {
                break;
            }
            match ws.read() {
                Ok(tungstenite::Message::Text(t)) => {
                    let Ok(val) = serde_json::from_str::<serde_json::Value>(&t) else { continue };
                    let empty = Vec::new();
                    let arr = val.as_array().unwrap_or(&empty);
                    match arr.first().and_then(|x| x.as_str()) {
                        // ["EVENT", <sub>, <event>] — verify before trusting a relay-supplied event.
                        Some("EVENT") => {
                            if let Some(ev) = arr.get(2).and_then(Event::from_json) {
                                if ev.verify() && seen.insert(ev.id.clone()) {
                                    out.push(ev);
                                }
                            }
                        }
                        Some("EOSE") => break, // end of stored events for this sub
                        _ => {}
                    }
                }
                Ok(tungstenite::Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
        let _ = ws.close(None);
    }
    out
}

// ── provenance attestation: the ACTOR's Ed25519 signature over a landed change, carried inside the
// schnorr-signed nostr event (kind 1900) — the dual-signature "bridge" (Ed25519 keel authority ⇄
// secp256k1 nostr transport): a wrapping, not a key derivation. A reader verifies BOTH: schnorr = "this
// instance transported it"; Ed25519 = "`claim.actor` authorized it".
//
// NOTE on the trust it actually delivers: this only becomes non-repudiable-by-the-human authorship for
// SOVEREIGN accounts (the client signs — the follow-up). For the custodial/demo path shipped here the
// instance holds BOTH keys, so the Ed25519 sig is really an instance attestation "we assert actor X
// authored C" — same trust domain as the schnorr sig. The primitives are identical either way, so the
// sovereign signer reuses them verbatim; the difference is only WHERE the actor secret lives. ──

/// Regular (append-only) provenance attestation event kind.
pub const KIND_PROV: u16 = 1900;

/// The actor's claim about a landed change. Field order is the canonical signing order — the Ed25519
/// signature is over `serde_json::to_string(claim)`, and a verifier recomputes the same string.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProvenanceClaim {
    pub v: u8,
    pub change: String,
    /// The actor's Ed25519 public key (its hull actor id) that signed this claim.
    pub actor: String,
    pub repo: String,
    pub intent: String,
    pub ts: u64,
}

impl ProvenanceClaim {
    /// The exact bytes the actor signs — a FLAT, domain-separated `key=value` form (like
    /// `identity::hop_message`), NOT JSON: `JSON.stringify` (the future in-browser sovereign signer) and
    /// `serde_json` disagree on number/escaping details, so a flat form is the only cross-language-stable
    /// choice. The `hull-provenance:v1` prefix separates it from `hull-login:` / `hull-delegation:v1` /
    /// `hull-sovereign:v1`. The free-text `intent` is signed by its SHA-256 (any bytes → fixed hex), so
    /// newlines/unicode in it can't break the line-delimited structure; the other fields are
    /// newline-free by construction (hex ids, a sanitized `tenant/repo`, a number).
    fn signing_bytes(&self) -> String {
        let intent_sha256 = hex::encode(Sha256::digest(self.intent.as_bytes()));
        format!(
            "hull-provenance:v1\nchange={}\nactor={}\nrepo={}\nintent_sha256={}\nts={}",
            self.change, self.actor, self.repo, intent_sha256, self.ts
        )
    }
}

/// A provenance claim plus the actor's Ed25519 signature over it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SignedProvenance {
    pub claim: ProvenanceClaim,
    pub ed_sig: String,
}

/// Sign a claim with the actor's Ed25519 secret. `None` unless the secret's public key equals
/// `claim.actor` — you can only attest as yourself (checked by verifying the fresh signature against
/// the claimed actor id).
pub fn sign_provenance(actor_secret_hex: &str, claim: ProvenanceClaim) -> Option<SignedProvenance> {
    let msg = claim.signing_bytes();
    let ed_sig = hull_core::identity::sign(actor_secret_hex, msg.as_bytes())?;
    hull_core::identity::verify_strict(&claim.actor, msg.as_bytes(), &ed_sig).then_some(SignedProvenance { claim, ed_sig })
}

/// Verify the Ed25519 half of a signed provenance bundle: `ed_sig` is a STRICT-valid signature for
/// `claim.actor` (strict rejects small-order/non-canonical attacker keys). Returns the attested actor
/// id. **This proves only that `claim.actor` authorized the claim — NOT that `claim.actor` is an
/// accountable hull actor.** A consumer that acts on provenance MUST additionally resolve
/// `claim.actor`'s delegation chain to a human in the actor store and check its authority over
/// `claim.repo`, and must trust ONLY the signed `claim` fields (never the event's schnorr pubkey or
/// tags, which a re-wrapper can set freely). No such consumer exists yet — this is the publish + verify
/// primitive only.
pub fn verify_provenance(sp: &SignedProvenance) -> Option<String> {
    hull_core::identity::verify_strict(&sp.claim.actor, sp.claim.signing_bytes().as_bytes(), &sp.ed_sig).then(|| sp.claim.actor.clone())
}

/// Build a kind:1900 provenance event: the actor-signed bundle in `content`, schnorr-signed by the
/// instance key for transport. Tags surface the change/repo/actor for filtering. `None` if either key
/// step fails.
pub fn prov_event(instance_secret_hex: &str, sp: &SignedProvenance, created_at: u64) -> Option<Event> {
    let content = serde_json::to_string(sp).ok()?;
    let tags = vec![
        vec!["t".to_string(), "keel-provenance".to_string()],
        vec!["change".to_string(), sp.claim.change.clone()],
        vec!["repo".to_string(), sp.claim.repo.clone()],
        vec!["actor".to_string(), sp.claim.actor.clone()],
    ];
    build_event(instance_secret_hex, created_at, KIND_PROV, tags, &content)
}

/// Fully verify a kind:1900 provenance event — schnorr (transport) AND the embedded Ed25519 actor sig.
/// Returns the attested actor id iff BOTH pass. This is what makes provenance on relays trustworthy
/// without trusting the relay: the actor's own signature travels with the change.
pub fn verify_prov_event(ev: &Event) -> Option<String> {
    if ev.kind != KIND_PROV || !ev.verify() {
        return None;
    }
    let sp: SignedProvenance = serde_json::from_str(&ev.content).ok()?;
    verify_provenance(&sp)
}

/// Publishes/reads repo refs as signed nostr events (kind 31900), decoupled from the notifier. Same
/// env config as [`NostrNotifier`] (`HULL_NOSTR_SECRET` + `HULL_NOSTR_RELAYS`); signed by the instance
/// key for transport authenticity. The Ed25519 keel-provenance bundle is a later increment.
/// One instance's view of a repo's branch: which instance key published it, and at what commit. Used
/// by [`NostrRefs::fetch_federated_ref`] to show where each federated instance says a branch points.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PeerRef {
    /// The publishing instance's x-only nostr pubkey (its transport identity).
    pub pubkey: String,
    pub commit: String,
    /// True if this is our own instance's ref (our key published it).
    pub is_self: bool,
}

/// True if `s` is a syntactically valid x-only (32-byte) nostr pubkey in hex.
fn valid_xonly(s: &str) -> bool {
    hex::decode(s.trim()).ok().filter(|b| b.len() == 32).and_then(|b| XOnlyPublicKey::from_slice(&b).ok()).is_some()
}

#[derive(Clone)]
pub struct NostrRefs {
    secret_hex: String,
    relays: Vec<String>,
    /// Peer instance pubkeys (x-only hex) we federate refs with. Trust is explicit and non-transitive:
    /// an instance appears here only because this instance was told to trust it. Each peer's refs are
    /// transport-signed by that peer's key, so reading one proves the peer published it (never a relay).
    peers: Vec<String>,
}

impl NostrRefs {
    pub fn from_env() -> Option<Self> {
        let secret_hex = std::env::var("HULL_NOSTR_SECRET").ok()?;
        pubkey_of(&secret_hex)?;
        let relays: Vec<String> = std::env::var("HULL_NOSTR_RELAYS")
            .ok()?
            .split([',', ' '])
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        // HULL_NOSTR_PEERS: comma/space-separated peer instance pubkeys (x-only hex). Malformed entries
        // are dropped with a warning rather than failing startup — one bad pubkey shouldn't sink the set.
        let peers: Vec<String> = std::env::var("HULL_NOSTR_PEERS")
            .ok()
            .unwrap_or_default()
            .split([',', ' '])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter_map(|s| {
                if valid_xonly(s) {
                    Some(s.to_string())
                } else {
                    eprintln!("nostr: ignoring malformed HULL_NOSTR_PEERS entry {s:?} (want 32-byte x-only hex)");
                    None
                }
            })
            .collect();
        (!relays.is_empty()).then_some(Self { secret_hex, relays, peers })
    }

    pub fn new(secret_hex: String, relays: Vec<String>) -> Self {
        Self { secret_hex, relays, peers: Vec::new() }
    }
    /// Add federated peer instance pubkeys (x-only hex); invalid entries are dropped.
    pub fn with_peers(mut self, peers: Vec<String>) -> Self {
        self.peers = peers.into_iter().filter(|p| valid_xonly(p)).collect();
        self
    }
    pub fn relays(&self) -> &[String] {
        &self.relays
    }
    pub fn peers(&self) -> &[String] {
        &self.peers
    }

    /// Publish `repo`'s `branch` → `commit` (best-effort across relays). Returns the signed event.
    pub fn publish_ref(&self, repo: &str, branch: &str, commit: &str, prev: Option<&str>) -> Option<Event> {
        let created_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let ev = ref_event(&self.secret_hex, repo, branch, commit, prev, created_at)?;
        publish(&self.relays, &ev);
        Some(ev)
    }

    /// Publish an ACTOR-signed provenance attestation (kind 1900) for a landed change: the actor's
    /// Ed25519 key (`actor_secret_hex`, held only for custodial/demo accounts — a sovereign actor
    /// signs client-side) signs the claim, wrapped in the instance's schnorr-signed event. Best-effort.
    pub fn publish_provenance(&self, actor_secret_hex: &str, change: &str, actor: &str, repo: &str, intent: &str) -> Option<Event> {
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let claim = ProvenanceClaim { v: 1, change: change.to_string(), actor: actor.to_string(), repo: repo.to_string(), intent: intent.to_string(), ts };
        let sp = sign_provenance(actor_secret_hex, claim)?;
        let ev = prov_event(&self.secret_hex, &sp, ts)?;
        publish(&self.relays, &ev);
        Some(ev)
    }

    /// Read back FULLY-VERIFIED provenance attestations for `repo` from the relays: each returned
    /// bundle passed both the schnorr (transport) and Ed25519 (actor) checks via [`verify_prov_event`],
    /// and its SIGNED `claim.repo` matches `repo` (we trust the signed field, never the mutable event
    /// tag). Accountability of `claim.actor` — its delegation chain to a human and authority over the
    /// repo — is the CALLER's job (it needs the actor store); this only guarantees the signatures.
    pub fn fetch_provenance(&self, repo: &str) -> Vec<SignedProvenance> {
        let filter = serde_json::json!({ "kinds": [KIND_PROV], "#repo": [repo] });
        fetch_events(&self.relays, filter)
            .iter()
            .filter(|ev| verify_prov_event(ev).is_some()) // schnorr + Ed25519 both valid
            .filter_map(|ev| serde_json::from_str::<SignedProvenance>(&ev.content).ok())
            .filter(|sp| sp.claim.repo == repo) // trust the SIGNED repo, not the relay-supplied tag/filter
            .collect()
    }

    /// Read the newest published commit for `repo`'s `branch` back from the relays (own-authored refs).
    pub fn fetch_ref(&self, repo: &str, branch: &str) -> Option<String> {
        let author = pubkey_of(&self.secret_hex)?;
        self.fetch_ref_from(&author, repo, branch)
    }

    /// Read the newest commit for `repo`'s `branch` published by a SPECIFIC instance key (`author`,
    /// x-only hex). Author + kind + d-tag are re-checked CLIENT-SIDE: a relay can ignore the REQ filter
    /// and hand back a validly-signed event by a different key, so a foreign event can never masquerade
    /// as this author's ref for this repo.
    pub fn fetch_ref_from(&self, author: &str, repo: &str, branch: &str) -> Option<String> {
        let dtag = format!("{repo}#{branch}");
        let filter = serde_json::json!({ "kinds": [KIND_REF], "authors": [author], "#d": [dtag] });
        let mine: Vec<Event> = fetch_events(&self.relays, filter)
            .into_iter()
            .filter(|e| e.pubkey == author && e.kind == KIND_REF && e.tags.iter().any(|t| t.len() == 2 && t[0] == "d" && t[1] == dtag))
            .collect();
        newest_commit(&mine)
    }

    /// Federated view of `repo`'s `branch`: the newest commit each trusted instance (ourselves plus the
    /// configured [`peers`](Self::peers)) says it points at. One relay sweep for the whole author set,
    /// then newest-wins per author. An instance that never published this ref is simply absent. Callers
    /// compare commits to spot divergence; each entry is transport-signed by that instance's own key.
    pub fn fetch_federated_ref(&self, repo: &str, branch: &str) -> Vec<PeerRef> {
        let own = pubkey_of(&self.secret_hex);
        // Trusted author set: self first, then peers, deduped (a peer list that repeats self is fine).
        let mut authors: Vec<String> = Vec::new();
        if let Some(a) = &own {
            authors.push(a.clone());
        }
        for p in &self.peers {
            if !authors.contains(p) {
                authors.push(p.clone());
            }
        }
        if authors.is_empty() {
            return Vec::new();
        }
        let dtag = format!("{repo}#{branch}");
        let filter = serde_json::json!({ "kinds": [KIND_REF], "authors": authors, "#d": [dtag] });
        // fetch_events already verified each event's schnorr sig; still re-check author ∈ our set, kind,
        // and d-tag client-side (a relay may return extra events the filter should have excluded).
        let events: Vec<Event> = fetch_events(&self.relays, filter)
            .into_iter()
            .filter(|e| e.kind == KIND_REF && authors.iter().any(|a| a == &e.pubkey) && e.tags.iter().any(|t| t.len() == 2 && t[0] == "d" && t[1] == dtag))
            .collect();
        let mut out: Vec<PeerRef> = Vec::new();
        for author in &authors {
            let per: Vec<Event> = events.iter().filter(|e| &e.pubkey == author).cloned().collect();
            if let Some(commit) = newest_commit(&per) {
                out.push(PeerRef { pubkey: author.clone(), commit, is_self: own.as_deref() == Some(author.as_str()) });
            }
        }
        out
    }
}

/// A [`Notifier`] that publishes Hull notifications to nostr relays, `p`-tagging each recipient who
/// opted in with a [`nostr_pubkey`](hull_core::Actor::nostr_pubkey). Config-gated (off unless a
/// publisher key + relays are set), so the OSS default stays the log notifier.
pub struct NostrNotifier {
    secret_hex: String,
    relays: Vec<String>,
    store: Arc<dyn Store>,
}

impl NostrNotifier {
    /// Build from env — `HULL_NOSTR_SECRET` (Hull's 32-byte publisher key, hex) and
    /// `HULL_NOSTR_RELAYS` (comma/space-separated wss urls). `None` if unconfigured or the key is bad.
    pub fn from_env(store: Arc<dyn Store>) -> Option<Self> {
        let secret_hex = std::env::var("HULL_NOSTR_SECRET").ok()?;
        pubkey_of(&secret_hex)?; // validate the key
        let relays: Vec<String> = std::env::var("HULL_NOSTR_RELAYS")
            .ok()?
            .split([',', ' '])
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        (!relays.is_empty()).then_some(Self { secret_hex, relays, store })
    }

    /// The relays this notifier publishes to (for a startup log line).
    pub fn relays(&self) -> &[String] {
        &self.relays
    }
}

impl NostrNotifier {
    /// Build the signed nostr note for a [`NotifyEvent`], or `None` if no recipient opted into nostr.
    /// Pure but for the store lookup — the testable heart of [`Notifier::notify`].
    async fn event_for(&self, event: &NotifyEvent, created_at: u64) -> Option<Event> {
        let mut recipients: Vec<String> = Vec::new();
        for id in &event.to {
            if let Some(pk) = self.store.actor(id).await.and_then(|a| a.nostr_pubkey) {
                recipients.push(pk);
            }
        }
        if recipients.is_empty() {
            return None;
        }
        let mut tags: Vec<Vec<String>> = recipients.iter().map(|pk| vec!["p".to_string(), pk.clone()]).collect();
        tags.push(vec!["t".to_string(), "hull".to_string()]);
        if let Some(ch) = &event.change {
            tags.push(vec!["change".to_string(), ch.clone()]); // the keel change id, so an agent owner can act
        }
        let content = match &event.change {
            Some(ch) => format!("[hull:{}] {} — keel change {ch}", event.kind, event.summary),
            None => format!("[hull:{}] {}", event.kind, event.summary),
        };
        build_event(&self.secret_hex, created_at, 1, tags, &content)
    }
}

#[async_trait::async_trait]
impl Notifier for NostrNotifier {
    async fn notify(&self, event: &NotifyEvent) {
        let created_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let Some(ev) = self.event_for(event, created_at).await else { return };
        // Fire-and-forget: relay round-trips must never block or fail the request path.
        let relays = self.relays.clone();
        std::thread::spawn(move || {
            let n = publish(&relays, &ev);
            eprintln!("nostr: published {} to {n}/{} relay(s)", &ev.id[..12], relays.len());
        });
    }
}

/// A minimal in-process NIP-01 relay for tests (this crate's nostr tests AND lib.rs's substrate test):
/// stores EVENTs, answers a REQ with all stored events + EOSE. Enough to prove the publish → relay →
/// REQ → read-back → verify → parse path end to end. Serves connections sequentially with a store
/// shared across them, so an event published on one connection is visible to a later REQ.
#[cfg(test)]
pub(crate) fn spawn_loopback_relay() -> String {
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let store: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let Ok(mut ws) = tungstenite::accept(stream) else { continue };
            while let Ok(msg) = ws.read() {
                let tungstenite::Message::Text(t) = msg else {
                    break; // Close / non-text → this connection is done
                };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else { continue };
                let a = v.as_array().cloned().unwrap_or_default();
                match a.first().and_then(|x| x.as_str()) {
                    Some("EVENT") => {
                        if let Some(ev) = a.get(1) {
                            store.lock().unwrap().push(ev.clone());
                        }
                    }
                    Some("REQ") => {
                        let sub = a.get(1).and_then(|x| x.as_str()).unwrap_or("").to_string();
                        for ev in store.lock().unwrap().iter() {
                            let _ = ws.send(tungstenite::Message::Text(serde_json::json!(["EVENT", sub, ev]).to_string()));
                        }
                        let _ = ws.send(tungstenite::Message::Text(serde_json::json!(["EOSE", sub]).to_string()));
                    }
                    _ => {}
                }
            }
        }
    });
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    // A deterministic 32-byte test secret.
    const SK: &str = "0000000000000000000000000000000000000000000000000000000000000001";

    #[test]
    fn pubkey_is_derived_deterministically() {
        assert_eq!(pubkey_of(SK), pubkey_of(SK));
        assert_eq!(pubkey_of(SK).unwrap().len(), 64);
        assert!(pubkey_of("not-hex").is_none());
        assert!(pubkey_of("00").is_none()); // wrong length
    }

    #[test]
    fn event_id_and_signature_verify() {
        let ev = build_event(SK, 1_700_000_000, 1, vec![vec!["p".into(), "abc".into()], vec!["t".into(), "hull".into()]], "code you own was touched").unwrap();
        assert_eq!(ev.pubkey, pubkey_of(SK).unwrap());
        assert_eq!(ev.id.len(), 64);
        assert_eq!(ev.sig.len(), 128);
        assert!(ev.verify(), "a freshly built event must verify its own id + schnorr sig");
    }

    #[test]
    fn tampering_breaks_verification() {
        let mut ev = build_event(SK, 1_700_000_000, 1, vec![], "hello").unwrap();
        ev.content = "tampered".into(); // id no longer matches the content
        assert!(!ev.verify());
    }

    #[tokio::test]
    async fn notifier_builds_a_signed_note_only_for_opted_in_recipients() {
        use hull_core::store::{InMemory, Store};
        use hull_core::{Actor, ActorKind, Lifetime};
        let store = InMemory::new();
        // An owner who opted into nostr, and one who didn't.
        let mut opted = Actor { id: "owner1".into(), kind: ActorKind::Human, lifetime: Lifetime::Static, handle: "mo".into(), delegation: None, nostr_pubkey: None, revoked: false };
        opted.nostr_pubkey = Some(pubkey_of(SK).unwrap());
        store.put_actor(opted).await;
        store.put_actor(Actor { id: "owner2".into(), kind: ActorKind::Human, lifetime: Lifetime::Static, handle: "no".into(), delegation: None, nostr_pubkey: None, revoked: false }).await;

        let n = NostrNotifier { secret_hex: SK.into(), relays: vec!["ws://127.0.0.1:1".into()], store: Arc::new(store) };
        // targets both owners, but only owner1 opted in → the note p-tags exactly owner1's key + change tag.
        let ev = NotifyEvent { kind: "code_owner_referenced".into(), to: vec!["owner1".into(), "owner2".into()], summary: "crates/x touched".into(), change: Some("blake3:abc".into()), repo: None, target_kind: None, target_number: None };
        let note = n.event_for(&ev, 1_700_000_000).await.expect("an opted-in recipient yields a note");
        assert!(note.verify());
        assert!(note.tags.contains(&vec!["p".to_string(), pubkey_of(SK).unwrap()]));
        assert!(note.tags.iter().filter(|t| t[0] == "p").count() == 1); // owner2 (no key) is not tagged
        assert!(note.tags.contains(&vec!["change".to_string(), "blake3:abc".to_string()]));

        // nobody opted in → no note at all.
        let none = NotifyEvent { kind: "x".into(), to: vec!["owner2".into()], summary: "y".into(), change: None, repo: None, target_kind: None, target_number: None };
        assert!(n.event_for(&none, 1_700_000_000).await.is_none());
    }

    #[test]
    fn id_matches_nip01_serialization() {
        // id = sha256 of [0,pubkey,created_at,kind,tags,content] compact JSON
        let ev = build_event(SK, 1_700_000_000, 1, vec![], "x").unwrap();
        let expect: [u8; 32] = Sha256::digest(super::serialize_for_id(&ev.pubkey, 1_700_000_000, 1, &[], "x").as_bytes()).into();
        assert_eq!(ev.id, hex::encode(expect));
    }

    #[test]
    fn ref_event_is_a_verifiable_replaceable_with_the_right_d_tag() {
        let ev = ref_event(SK, "tankrap/hull", "main", "commitABC", Some("commitPREV"), 1_700_000_000).unwrap();
        assert_eq!(ev.kind, KIND_REF);
        assert!(ev.verify(), "ref event must verify its own id + schnorr sig");
        // the parameterized-replaceable key is `repo#branch`
        assert!(ev.tags.contains(&vec!["d".to_string(), "tankrap/hull#main".to_string()]));
        assert!(ev.tags.contains(&vec!["ref".to_string(), "main".to_string()]));
        assert!(ev.tags.contains(&vec!["prev".to_string(), "commitPREV".to_string()]));
        // the commit is carried in content
        let c: serde_json::Value = serde_json::from_str(&ev.content).unwrap();
        assert_eq!(c["commit"], "commitABC");
    }

    #[test]
    fn newest_commit_and_from_json_round_trip() {
        // latest-wins by created_at across a set of same-ref events
        let older = ref_event(SK, "r", "main", "old", None, 1_700_000_000).unwrap();
        let newer = ref_event(SK, "r", "main", "new", Some("old"), 1_700_000_100).unwrap();
        assert_eq!(newest_commit(&[older.clone(), newer.clone()]).as_deref(), Some("new"));
        assert_eq!(newest_commit(&[newer, older]).as_deref(), Some("new"), "order-independent");
        assert_eq!(newest_commit(&[]), None);
        // to_json → from_json preserves everything (and the event still verifies)
        let ev = ref_event(SK, "r", "dev", "xyz", None, 1_700_000_000).unwrap();
        let back = Event::from_json(&ev.to_json()).unwrap();
        assert_eq!((back.id.clone(), back.content.clone()), (ev.id.clone(), ev.content.clone()));
        assert!(back.verify());
    }


    #[test]
    fn provenance_carries_a_verifiable_actor_signature() {
        // The actor signs the claim with its Ed25519 key; the event is schnorr-signed by the instance.
        // A mint gives us a matching (actor id = ed pubkey, secret) pair.
        let actor = hull_core::identity::mint_human("agent");
        let claim = ProvenanceClaim {
            v: 1,
            change: "blake3:abc".into(),
            actor: actor.actor.id.clone(),
            repo: "tankrap/hull".into(),
            intent: "land the thing".into(),
            ts: 1_700_000_000,
        };
        let sp = sign_provenance(&actor.secret_key, claim).expect("actor signs its own claim");
        assert_eq!(verify_provenance(&sp).as_deref(), Some(actor.actor.id.as_str()));

        // wrap in a kind:1900 event (schnorr by the INSTANCE key SK) and verify BOTH sigs
        let ev = prov_event(SK, &sp, 1_700_000_000).unwrap();
        assert_eq!(ev.kind, KIND_PROV);
        assert_eq!(verify_prov_event(&ev).as_deref(), Some(actor.actor.id.as_str()), "schnorr + ed25519 both verify");
        assert!(ev.tags.contains(&vec!["change".to_string(), "blake3:abc".to_string()]));

        // you can't attest as someone else: signing with a different secret than claim.actor fails.
        let other = hull_core::identity::mint_human("other");
        let bad = ProvenanceClaim { actor: actor.actor.id.clone(), ..sp.claim.clone() };
        assert!(sign_provenance(&other.secret_key, bad).is_none(), "secret must match claim.actor");

        // tampering the content breaks verification (the ed_sig no longer covers it).
        let mut tampered = ev.clone();
        let mut sp2 = sp.clone();
        sp2.claim.change = "blake3:evil".into();
        tampered.content = serde_json::to_string(&sp2).unwrap();
        assert!(verify_prov_event(&tampered).is_none(), "content tamper (even if re-serialized) fails the schnorr id check");
    }

    #[test]
    fn publish_then_fetch_ref_round_trips_through_a_relay() {
        let url = spawn_loopback_relay();
        let refs = NostrRefs::new(SK.into(), vec![url]);
        // nothing published yet → no ref
        assert_eq!(refs.fetch_ref("tankrap/hull", "main"), None);
        // publish a ref, then read the commit back over a fresh connection
        let ev = refs.publish_ref("tankrap/hull", "main", "deadbeefcommit", None).expect("publish builds an event");
        assert!(ev.verify());
        assert_eq!(refs.fetch_ref("tankrap/hull", "main").as_deref(), Some("deadbeefcommit"), "the published commit reads back");
    }

    #[test]
    fn federation_reads_refs_across_trusted_peer_instances() {
        const SK2: &str = "0000000000000000000000000000000000000000000000000000000000000002";
        const SK3: &str = "0000000000000000000000000000000000000000000000000000000000000003";
        let url = spawn_loopback_relay();
        // A trusts B as a peer; A and B publish the same repo#branch at different commits.
        let a = NostrRefs::new(SK.into(), vec![url.clone()]).with_peers(vec![pubkey_of(SK2).unwrap()]);
        let b = NostrRefs::new(SK2.into(), vec![url.clone()]);
        a.publish_ref("tankrap/hull", "main", "commitA", None).unwrap();
        b.publish_ref("tankrap/hull", "main", "commitB", None).unwrap();

        let fed = a.fetch_federated_ref("tankrap/hull", "main");
        assert_eq!(fed.len(), 2, "self + one peer; got {fed:?}");
        let me = fed.iter().find(|p| p.is_self).expect("our own ref is present");
        assert_eq!(me.commit, "commitA");
        assert_eq!(me.pubkey, pubkey_of(SK).unwrap());
        let peer = fed.iter().find(|p| !p.is_self).expect("the peer's ref is present");
        assert_eq!(peer.commit, "commitB");
        assert_eq!(peer.pubkey, pubkey_of(SK2).unwrap());

        // An instance we do NOT list as a peer publishes the same ref — it must not appear.
        let c = NostrRefs::new(SK3.into(), vec![url.clone()]);
        c.publish_ref("tankrap/hull", "main", "commitC", None).unwrap();
        let fed2 = a.fetch_federated_ref("tankrap/hull", "main");
        assert_eq!(fed2.len(), 2, "an untrusted instance is not federated in");
        assert!(fed2.iter().all(|p| p.commit != "commitC"));

        // malformed peer pubkeys are dropped by with_peers.
        let d = NostrRefs::new(SK.into(), vec![url]).with_peers(vec!["not-hex".into(), "00".into()]);
        assert!(d.peers().is_empty());
    }

    #[test]
    fn publish_then_fetch_provenance_round_trips_and_rejects_forgeries() {
        let url = spawn_loopback_relay();
        let refs = NostrRefs::new(SK.into(), vec![url.clone()]);
        let actor = hull_core::identity::mint_human("agent");
        // publish an actor-signed provenance attestation, then read it back fully-verified.
        refs.publish_provenance(&actor.secret_key, "blake3:c1", &actor.actor.id, "tankrap/hull", "did a thing")
            .expect("publish");
        let got = refs.fetch_provenance("tankrap/hull");
        assert_eq!(got.len(), 1, "one verified attestation; got {got:?}");
        assert_eq!(got[0].claim.change, "blake3:c1");
        assert_eq!(got[0].claim.actor, actor.actor.id);

        // a forgery: publish a raw event whose SIGNED claim.repo is a DIFFERENT repo — fetch_provenance
        // for "tankrap/hull" must not return it (it trusts the signed claim, and here the ed_sig is over
        // a mismatched claim so verify_prov_event also rejects the tampered pairing).
        let other = hull_core::identity::mint_human("attacker");
        refs.publish_provenance(&other.secret_key, "blake3:evil", &other.actor.id, "someone/else", "evil")
            .expect("publish");
        let still = refs.fetch_provenance("tankrap/hull");
        assert!(still.iter().all(|sp| sp.claim.change != "blake3:evil"), "an attestation for another repo isn't returned");
    }
}
