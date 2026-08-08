//! Actor identity (M1). An actor's id **is** its Ed25519 public key, so authorship is a keypair,
//! not a claim. Minting enforces the hard invariant from [`crate::Actor`]: an agent MUST carry a
//! delegation chain that roots at a human, or it can't be minted.
//!
//! **The crypto layer (this module).** A [`Delegation`] is a biscuit-style, attenuation-only chain
//! of hops; each hop is an Ed25519 signature *by the parent principal* authorizing the child with a
//! narrowed scope and a non-increasing TTL. [`Delegation::verify`] is the gate every authoring
//! boundary runs: it checks each hop's parent-signature, that scope only narrows, a depth cap, TTL
//! expiry, and revocation — and returns the human at the root. A chain whose hops aren't validly
//! signed by their parents is **unaccountable**, exactly as an un-rooted one is. Revoking any
//! principal invalidates every descendant automatically (its id appears in their chains).

use crate::{Actor, ActorId, ActorKind, Delegation, DelegationHop, Lifetime};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;

/// Max hops in a delegation chain (human root + agents). A depth cap bounds blast radius and stops
/// unbounded re-delegation.
pub const MAX_DELEGATION_DEPTH: usize = 8;

/// Sign `message` with a hex Ed25519 secret key, returning the hex signature — the counterpart to
/// [`verify`]. `None` if `secret_hex` isn't a 32-byte hex key. For sovereign accounts signing happens
/// client-side (Hull never holds the secret); this exists for the demo/CLI paths and tests.
pub fn sign(secret_hex: &str, message: &[u8]) -> Option<String> {
    let bytes = hex::decode(secret_hex).ok()?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    Some(hex::encode(SigningKey::from_bytes(&arr).sign(message).to_bytes()))
}

/// Verify that `signature_hex` is a valid Ed25519 signature of `message` by the actor whose id is
/// `actor_id_hex` (the id **is** the public key). This is how an actor proves it holds the private
/// key at login — authorship becomes cryptographic, not a claim.
pub fn verify(actor_id_hex: &str, message: &[u8], signature_hex: &str) -> bool {
    let Ok(sig) = hex::decode(signature_hex) else { return false };
    verify_bytes(actor_id_hex, message, &sig)
}

/// [`verify`] over raw signature bytes (delegation hops store `Vec<u8>`, not hex).
pub fn verify_bytes(actor_id_hex: &str, message: &[u8], signature: &[u8]) -> bool {
    let Ok(pk) = hex::decode(actor_id_hex) else { return false };
    let Ok(pk): Result<[u8; 32], _> = pk.try_into() else { return false };
    let Ok(vk) = VerifyingKey::from_bytes(&pk) else { return false };
    let Ok(sig): Result<[u8; 64], _> = signature.try_into() else { return false };
    vk.verify(message, &Signature::from_bytes(&sig)).is_ok()
}

/// Like [`verify`] but STRICT: rejects small-order / non-canonical public keys (ed25519-dalek's
/// `verify_strict`). Use where the key is attacker-supplied and being *bound to an identity* — e.g. a
/// sovereign account's proof-of-possession — so nobody can claim a small-order id whose signatures
/// anyone can forge. (The lax [`verify`] stays for existing delegation-chain checks.)
pub fn verify_strict(actor_id_hex: &str, message: &[u8], signature_hex: &str) -> bool {
    let Ok(pk) = hex::decode(actor_id_hex) else { return false };
    let Ok(pk): Result<[u8; 32], _> = pk.try_into() else { return false };
    let Ok(vk) = VerifyingKey::from_bytes(&pk) else { return false };
    let Ok(sig) = hex::decode(signature_hex) else { return false };
    let Ok(sig): Result<[u8; 64], _> = sig.try_into() else { return false };
    vk.verify_strict(message, &Signature::from_bytes(&sig)).is_ok()
}

/// The canonical bytes a **parent** signs to delegate to a child — versioned so the format can
/// evolve. Binding parent+child+kind+scope+expiry means a signature can't be replayed onto a
/// different child, a widened scope, or a longer TTL.
pub fn hop_message(parent_id: &str, child_id: &str, kind: ActorKind, scope: &str, expires_unix: u64) -> Vec<u8> {
    let k = match kind {
        ActorKind::Human => "human",
        ActorKind::Agent => "agent",
    };
    format!("hull-delegation:v1\nparent={parent_id}\nchild={child_id}\nkind={k}\nscope={scope}\nexpires={expires_unix}").into_bytes()
}

/// Parent signs a delegation hop with its secret key (hex). Returns the signature bytes, or `None`
/// if the secret isn't a valid 32-byte key. The signer's public key must equal `parent_id`.
pub fn sign_hop(parent_secret_hex: &str, parent_id: &str, child_id: &str, kind: ActorKind, scope: &str, expires_unix: u64) -> Option<Vec<u8>> {
    let bytes = hex::decode(parent_secret_hex).ok()?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    let sk = SigningKey::from_bytes(&arr);
    if hex::encode(sk.verifying_key().to_bytes()) != parent_id {
        return None; // the secret doesn't correspond to the claimed parent — refuse to sign
    }
    let msg = hop_message(parent_id, child_id, kind, scope, expires_unix);
    Some(sk.sign(&msg).to_bytes().to_vec())
}

/// Why a delegation failed verification — surfaced so an authoring boundary can say precisely why an
/// actor was rejected (never silently treat an unverifiable chain as accountable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegationError {
    Empty,
    NotHumanRooted,
    TooDeep,
    /// The chain's last hop isn't the actor presenting it.
    TerminalMismatch,
    /// Hop `index` widened the parent's scope instead of narrowing it.
    ScopeWidened(usize),
    /// Hop `index` (or the root) is past its TTL.
    Expired(usize),
    /// A principal in the chain has been revoked (kills the whole subtree).
    Revoked(ActorId),
    /// Hop `index` is not validly signed by its parent principal.
    BadSignature(usize),
}

impl std::fmt::Display for DelegationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DelegationError::Empty => write!(f, "empty delegation chain"),
            DelegationError::NotHumanRooted => write!(f, "delegation does not root at a human"),
            DelegationError::TooDeep => write!(f, "delegation chain exceeds the depth cap"),
            DelegationError::TerminalMismatch => write!(f, "delegation does not terminate at this actor"),
            DelegationError::ScopeWidened(i) => write!(f, "hop {i} widens the parent's scope"),
            DelegationError::Expired(i) => write!(f, "hop {i} has expired"),
            DelegationError::Revoked(id) => write!(f, "principal {id} is revoked"),
            DelegationError::BadSignature(i) => write!(f, "hop {i} is not validly signed by its parent"),
        }
    }
}

/// True iff `child` scope is a subset of `parent` scope — every child grant must be covered by some
/// parent grant. Scopes are whitespace/comma-separated globs; `*` (or a trailing `pfx*`) covers.
/// This is the attenuation rule: a hop may only narrow.
pub fn scope_covers(parent: &str, child: &str) -> bool {
    let split = |s: &str| -> Vec<String> { s.split([' ', ',', '\t']).filter(|t| !t.is_empty()).map(str::to_string).collect() };
    let (pg, cg) = (split(parent), split(child));
    if pg.is_empty() {
        return cg.is_empty(); // no parent grant covers nothing
    }
    cg.iter().all(|c| pg.iter().any(|p| glob_covers(p, c)))
}

/// Does glob `pattern` cover grant `token`? `*` covers anything; `pfx*` covers any `pfx…`; else exact.
fn glob_covers(pattern: &str, token: &str) -> bool {
    if pattern == "*" || pattern == token {
        return true;
    }
    match pattern.strip_suffix('*') {
        Some(prefix) => token.starts_with(prefix),
        None => false,
    }
}

impl Delegation {
    /// **The cryptographic accountability gate.** Verify this chain roots at a human and every hop is
    /// a valid, non-widening, unexpired, unrevoked delegation signed by its parent — and that it
    /// terminates at `actor_id`. Returns the human root's id on success. `now_unix` is the clock for
    /// TTL; `is_revoked(id)` reports whether a principal has been revoked.
    pub fn verify(&self, actor_id: &str, now_unix: u64, is_revoked: &dyn Fn(&str) -> bool) -> Result<ActorId, DelegationError> {
        let chain = &self.chain;
        if chain.is_empty() {
            return Err(DelegationError::Empty);
        }
        if chain.len() > MAX_DELEGATION_DEPTH {
            return Err(DelegationError::TooDeep);
        }
        let root = &chain[0];
        if root.kind != ActorKind::Human {
            return Err(DelegationError::NotHumanRooted);
        }
        if chain.last().map(|h| h.principal.as_str()) != Some(actor_id) {
            return Err(DelegationError::TerminalMismatch);
        }
        if is_revoked(&root.principal) {
            return Err(DelegationError::Revoked(root.principal.clone()));
        }
        if root.expires_unix != 0 && now_unix > root.expires_unix {
            return Err(DelegationError::Expired(0));
        }
        for i in 1..chain.len() {
            let parent = &chain[i - 1];
            let hop = &chain[i];
            if !scope_covers(&parent.scope, &hop.scope) {
                return Err(DelegationError::ScopeWidened(i));
            }
            // TTL is non-increasing: a hop may not outlive its parent (0 = no expiry = longest).
            let parent_exp = if parent.expires_unix == 0 { u64::MAX } else { parent.expires_unix };
            let hop_exp = if hop.expires_unix == 0 { u64::MAX } else { hop.expires_unix };
            if hop_exp > parent_exp {
                return Err(DelegationError::Expired(i));
            }
            if hop.expires_unix != 0 && now_unix > hop.expires_unix {
                return Err(DelegationError::Expired(i));
            }
            if is_revoked(&hop.principal) {
                return Err(DelegationError::Revoked(hop.principal.clone()));
            }
            let msg = hop_message(&parent.principal, &hop.principal, hop.kind, &hop.scope, hop.expires_unix);
            if !verify_bytes(&parent.principal, &msg, &hop.signature) {
                return Err(DelegationError::BadSignature(i));
            }
        }
        Ok(root.principal.clone())
    }
}

/// A freshly minted identity: the public [`Actor`] Hull stores, plus the Ed25519 **secret key**
/// (hex) returned to the caller ONCE. Hull never persists the secret.
pub struct Minted {
    pub actor: Actor,
    pub secret_key: String,
}

/// A cryptographically-random hex string of `bytes` bytes — for login nonces and session tokens.
pub fn random_hex(bytes: usize) -> String {
    use rand::RngCore;
    let mut b = vec![0u8; bytes];
    OsRng.fill_bytes(&mut b);
    hex::encode(b)
}

fn keypair() -> (String, String) {
    let sk = SigningKey::generate(&mut OsRng);
    (hex::encode(sk.verifying_key().to_bytes()), hex::encode(sk.to_bytes()))
}

/// Mint a human actor with a fresh keypair. A human is its own accountability root.
pub fn mint_human(handle: &str) -> Minted {
    let (id, secret_key) = keypair();
    let actor = Actor {
        id,
        kind: ActorKind::Human,
        lifetime: Lifetime::Static,
        handle: handle.to_string(),
        delegation: None,
        nostr_pubkey: None,
        revoked: false,
    };
    Minted { actor, secret_key }
}

/// Build a human actor from a client-held **public** key (hex) — for a SOVEREIGN (non-custodial)
/// account where Hull never sees the secret. Returns `None` if `pubkey_hex` isn't a valid 32-byte
/// Ed25519 verifying key. The caller must separately verify a signature by this key (proof of
/// possession) before trusting the binding — this only validates the key's shape and builds the actor.
pub fn human_from_pubkey(handle: &str, pubkey_hex: &str) -> Option<Actor> {
    let bytes = hex::decode(pubkey_hex).ok()?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    // Reject a non-canonical / non-curve point so the id is always a usable verifying key.
    VerifyingKey::from_bytes(&arr).ok()?;
    Some(Actor {
        id: hex::encode(arr),
        kind: ActorKind::Human,
        lifetime: Lifetime::Static,
        handle: handle.to_string(),
        delegation: None,
        nostr_pubkey: None,
        revoked: false,
    })
}

/// Mint a human from a **known** 32-byte secret key (hex) rather than a fresh random one — for a
/// deterministic identity whose key is shared with a client (e.g. a demo account that supports
/// one-click login through the real signature flow). Returns `None` if the secret isn't 32 bytes.
pub fn human_from_secret(handle: &str, secret_hex: &str) -> Option<Minted> {
    let bytes = hex::decode(secret_hex).ok()?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    let sk = SigningKey::from_bytes(&arr);
    let actor = Actor {
        id: hex::encode(sk.verifying_key().to_bytes()),
        kind: ActorKind::Human,
        lifetime: Lifetime::Static,
        handle: handle.to_string(),
        delegation: None,
        nostr_pubkey: None,
        revoked: false,
    };
    Some(Minted { actor, secret_key: secret_hex.to_string() })
}

/// The expiry a lifetime implies for a delegation hop (`0` = no expiry).
fn lifetime_expiry(lifetime: Lifetime) -> u64 {
    match lifetime {
        Lifetime::Ephemeral { expires_unix } => expires_unix,
        Lifetime::Static => 0,
    }
}

/// Extend `parent`'s chain with a new agent hop, given the hop's parent-signature. Shared by the
/// server-side ([`mint_agent`]) and client-signed ([`delegate`]) paths. The new hop is verified as a
/// fail-fast at mint (valid parent signature, scope only narrows, depth cap); revocation/TTL are
/// re-checked at every authoring boundary by [`Delegation::verify`].
fn assemble_agent(handle: &str, parent: &Actor, child_id: &str, scope: &str, lifetime: Lifetime, signature: Vec<u8>) -> Option<Actor> {
    parent.human_principal()?; // structural: parent must resolve to a human, else refuse
    let expires_unix = lifetime_expiry(lifetime);
    let mut chain = match &parent.delegation {
        Some(d) => d.chain.clone(),
        None => vec![DelegationHop { principal: parent.id.clone(), kind: parent.kind, scope: "*".to_string(), expires_unix: 0, signature: vec![] }],
    };
    chain.push(DelegationHop { principal: child_id.to_string(), kind: ActorKind::Agent, scope: scope.to_string(), expires_unix, signature });
    let actor = Actor { id: child_id.to_string(), kind: ActorKind::Agent, lifetime, handle: handle.to_string(), delegation: Some(Delegation { chain }), nostr_pubkey: None , revoked: false };
    // Fail-fast: the assembled chain must verify (signature + attenuation + depth + human root). Use
    // now=0 (don't reject on TTL at mint) and no revocations (nothing is revoked the instant it mints).
    actor.delegation.as_ref()?.verify(child_id, 0, &|_| false).ok()?;
    Some(actor)
}

/// Mint an agent delegated by `parent`, signing the new hop with `parent_secret_hex` (which MUST be
/// the parent's key). Server-side/test path: Hull generates the child keypair. Returns `None` if the
/// parent is unaccountable or the secret doesn't match the parent. The chain roots at a human —
/// "no agent without a human root" — and is now cryptographically signed, not merely asserted.
pub fn mint_agent(handle: &str, parent: &Actor, parent_secret_hex: &str, scope: &str, lifetime: Lifetime) -> Option<Minted> {
    let (id, secret_key) = keypair();
    let sig = sign_hop(parent_secret_hex, &parent.id, &id, ActorKind::Agent, scope, lifetime_expiry(lifetime))?;
    let actor = assemble_agent(handle, parent, &id, scope, lifetime, sig)?;
    Some(Minted { actor, secret_key })
}

/// Client-signed delegation: the caller generated the child keypair and signed the hop with the
/// **parent's** key; Hull only assembles and verifies, never seeing the child secret. This is how a
/// human delegates from the web without handing Hull their private key. `signature` is the parent's
/// Ed25519 signature over [`hop_message`]. Returns the public [`Actor`] to store, or `None` if the
/// chain doesn't verify (bad signature / widened scope / unaccountable parent).
pub fn delegate(handle: &str, parent: &Actor, child_id: &str, scope: &str, lifetime: Lifetime, signature: Vec<u8>) -> Option<Actor> {
    assemble_agent(handle, parent, child_id, scope, lifetime, signature)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_is_its_own_root_with_a_real_key() {
        let h = mint_human("justin");
        assert_eq!(h.actor.kind, ActorKind::Human);
        assert_eq!(h.actor.id.len(), 64); // 32-byte pubkey, hex
        assert_eq!(h.secret_key.len(), 64);
        assert_eq!(h.actor.human_principal(), Some(&h.actor.id));
    }

    fn no_revocations(_: &str) -> bool {
        false
    }

    #[test]
    fn agent_chains_to_the_delegating_human() {
        let human = mint_human("justin");
        let agent = mint_agent("agent:reviewer", &human.actor, &human.secret_key, "issues:*", Lifetime::Ephemeral { expires_unix: 0 })
            .expect("agent delegated by a human is accountable");
        assert!(agent.actor.is_accountable());
        assert_eq!(agent.actor.human_principal(), Some(&human.actor.id));
        // and it CRYPTOGRAPHICALLY verifies — the hop is signed by the human.
        assert_eq!(agent.actor.delegation.as_ref().unwrap().verify(&agent.actor.id, 0, &no_revocations), Ok(human.actor.id.clone()));

        // multi-hop: an agent delegating a sub-agent (narrower scope) still roots at the human and verifies.
        let sub = mint_agent("agent:fix", &agent.actor, &agent.secret_key, "issues:close", Lifetime::Static).unwrap();
        assert_eq!(sub.actor.human_principal(), Some(&human.actor.id));
        assert_eq!(sub.actor.delegation.as_ref().unwrap().verify(&sub.actor.id, 0, &no_revocations), Ok(human.actor.id.clone()));
    }

    #[test]
    fn a_parent_cannot_sign_with_the_wrong_key() {
        let human = mint_human("justin").actor;
        let impostor = mint_human("mallory");
        // minting an agent "from justin" but signing with mallory's key must fail (key ≠ parent id)
        assert!(mint_agent("agent:x", &human, &impostor.secret_key, "*", Lifetime::Static).is_none());
    }

    #[test]
    fn tampering_breaks_verification() {
        let human = mint_human("justin");
        let mut agent = mint_agent("agent:reviewer", &human.actor, &human.secret_key, "issues:*", Lifetime::Static).unwrap().actor;
        // widen the scope after signing → signature no longer matches the (parent,child,scope) bytes
        agent.delegation.as_mut().unwrap().chain[1].scope = "*".into();
        assert_eq!(agent.delegation.as_ref().unwrap().verify(&agent.id, 0, &no_revocations), Err(DelegationError::BadSignature(1)));
    }

    #[test]
    fn revocation_propagates_to_descendants() {
        let human = mint_human("justin");
        let agent = mint_agent("agent:reviewer", &human.actor, &human.secret_key, "*", Lifetime::Static).unwrap();
        let sub = mint_agent("agent:fix", &agent.actor, &agent.secret_key, "*", Lifetime::Static).unwrap();
        // revoke the middle agent → the sub-agent (whose chain contains it) is no longer accountable.
        let revoked = |id: &str| id == agent.actor.id;
        assert_eq!(sub.actor.delegation.as_ref().unwrap().verify(&sub.actor.id, 0, &revoked), Err(DelegationError::Revoked(agent.actor.id.clone())));
        // revoking the human root kills the direct agent too.
        let revoke_human = |id: &str| id == human.actor.id;
        assert!(agent.actor.delegation.as_ref().unwrap().verify(&agent.actor.id, 0, &revoke_human).is_err());
    }

    #[test]
    fn expired_hop_is_rejected() {
        let human = mint_human("justin");
        let agent = mint_agent("agent:reviewer", &human.actor, &human.secret_key, "*", Lifetime::Ephemeral { expires_unix: 1000 }).unwrap();
        assert!(agent.actor.delegation.as_ref().unwrap().verify(&agent.actor.id, 500, &no_revocations).is_ok()); // before expiry
        assert_eq!(agent.actor.delegation.as_ref().unwrap().verify(&agent.actor.id, 2000, &no_revocations), Err(DelegationError::Expired(1))); // after
    }

    #[test]
    fn client_signed_delegation_matches_server_minted() {
        // the human generates a child keypair client-side and signs the hop — Hull never sees the secret
        let human = mint_human("justin");
        let child = mint_human("throwaway"); // reuse keygen to get a keypair; child is used as an agent id
        let sig = sign_hop(&human.secret_key, &human.actor.id, &child.actor.id, ActorKind::Agent, "review:*", 0).unwrap();
        let agent = delegate("agent:web", &human.actor, &child.actor.id, "review:*", Lifetime::Static, sig).expect("client-signed delegation verifies");
        assert_eq!(agent.delegation.as_ref().unwrap().verify(&agent.id, 0, &no_revocations), Ok(human.actor.id.clone()));
    }

    #[test]
    fn scope_attenuation_rules() {
        assert!(scope_covers("*", "issues:close"));
        assert!(scope_covers("issues:*", "issues:close"));
        assert!(scope_covers("issues:* review:*", "review:approve"));
        assert!(!scope_covers("issues:close", "issues:*")); // child widens → not covered
        assert!(!scope_covers("issues:*", "prs:merge")); // different prefix
    }

    #[test]
    fn signature_verifies_key_possession() {
        // sign a message with the minted secret, verify against the actor id (public key)
        let m = mint_human("justin");
        let sk_bytes: [u8; 32] = hex::decode(&m.secret_key).unwrap().try_into().unwrap();
        let sk = SigningKey::from_bytes(&sk_bytes);
        let msg = b"hull-login:nonce-123";
        let sig = ed25519_dalek::Signer::sign(&sk, msg);
        assert!(verify(&m.actor.id, msg, &hex::encode(sig.to_bytes())));
        assert!(!verify(&m.actor.id, b"different message", &hex::encode(sig.to_bytes())));
        // a different actor's id must not verify this signature
        let other = mint_human("other").actor.id;
        assert!(!verify(&other, msg, &hex::encode(sig.to_bytes())));
    }

    #[test]
    fn cannot_mint_an_agent_from_an_unaccountable_parent() {
        // an agent with no delegation is unaccountable; delegating from it must be refused
        let orphan = Actor {
            id: "deadbeef".into(),
            kind: ActorKind::Agent,
            lifetime: Lifetime::Static,
            handle: "orphan".into(),
            delegation: None,
            nostr_pubkey: None,
            revoked: false,
        };
        // even with a syntactically-valid secret, an unaccountable parent is refused.
        assert!(mint_agent("agent:child", &orphan, &"11".repeat(32), "*", Lifetime::Static).is_none());
    }
}
