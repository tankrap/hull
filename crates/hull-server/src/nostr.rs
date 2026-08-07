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
                if let tungstenite::stream::MaybeTlsStream::Plain(s) = ws.get_ref() {
                    let _ = s.set_write_timeout(Some(Duration::from_secs(4)));
                }
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
    fn event_for(&self, event: &NotifyEvent, created_at: u64) -> Option<Event> {
        // TODO(async-store): the `Store` trait is now async, but `Notifier::notify` (which calls this)
        // is a synchronous external trait method, so we can't `.await` here. Bridge to the current
        // tokio runtime with `block_in_place` + `block_on`. `notify` is always dispatched from an async
        // request handler on the multi-threaded server runtime, where `block_in_place` is valid. If a
        // future caller invokes `notify` off-runtime this will panic — making `Notifier` async is the
        // real fix.
        let recipients: Vec<String> = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut out = Vec::new();
                for id in &event.to {
                    if let Some(pk) = self.store.actor(id).await.and_then(|a| a.nostr_pubkey) {
                        out.push(pk);
                    }
                }
                out
            })
        });
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

impl Notifier for NostrNotifier {
    fn notify(&self, event: &NotifyEvent) {
        let created_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let Some(ev) = self.event_for(event, created_at) else { return };
        // Fire-and-forget: relay round-trips must never block or fail the request path.
        let relays = self.relays.clone();
        std::thread::spawn(move || {
            let n = publish(&relays, &ev);
            eprintln!("nostr: published {} to {n}/{} relay(s)", &ev.id[..12], relays.len());
        });
    }
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

    // `event_for` bridges its now-async store lookup with `block_in_place`, which requires a
    // multi-threaded runtime — hence `flavor = "multi_thread"`.
    #[tokio::test(flavor = "multi_thread")]
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
        let note = n.event_for(&ev, 1_700_000_000).expect("an opted-in recipient yields a note");
        assert!(note.verify());
        assert!(note.tags.contains(&vec!["p".to_string(), pubkey_of(SK).unwrap()]));
        assert!(note.tags.iter().filter(|t| t[0] == "p").count() == 1); // owner2 (no key) is not tagged
        assert!(note.tags.contains(&vec!["change".to_string(), "blake3:abc".to_string()]));

        // nobody opted in → no note at all.
        let none = NotifyEvent { kind: "x".into(), to: vec!["owner2".into()], summary: "y".into(), change: None, repo: None, target_kind: None, target_number: None };
        assert!(n.event_for(&none, 1_700_000_000).is_none());
    }

    #[test]
    fn id_matches_nip01_serialization() {
        // id = sha256 of [0,pubkey,created_at,kind,tags,content] compact JSON
        let ev = build_event(SK, 1_700_000_000, 1, vec![], "x").unwrap();
        let expect: [u8; 32] = Sha256::digest(super::serialize_for_id(&ev.pubkey, 1_700_000_000, 1, &[], "x").as_bytes()).into();
        assert_eq!(ev.id, hex::encode(expect));
    }
}
