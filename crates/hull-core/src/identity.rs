//! Actor identity (M1). An actor's id **is** its Ed25519 public key, so authorship is a keypair,
//! not a claim. Minting enforces the hard invariant from [`crate::Actor`]: an agent MUST carry a
//! delegation chain that roots at a human, or it can't be minted.
//!
//! Per-action signatures (signing each comment/review/commit and verifying the attenuation chain)
//! are the next layer; this establishes the identities and the structural accountability gate.

use crate::{Actor, ActorKind, Delegation, DelegationHop, Lifetime};
use ed25519_dalek::{Signature, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;

/// Verify that `signature_hex` is a valid Ed25519 signature of `message` by the actor whose id is
/// `actor_id_hex` (the id **is** the public key). This is how an actor proves it holds the private
/// key at login — authorship becomes cryptographic, not a claim.
pub fn verify(actor_id_hex: &str, message: &[u8], signature_hex: &str) -> bool {
    let Ok(pk) = hex::decode(actor_id_hex) else { return false };
    let Ok(pk): Result<[u8; 32], _> = pk.try_into() else { return false };
    let Ok(vk) = VerifyingKey::from_bytes(&pk) else { return false };
    let Ok(sig) = hex::decode(signature_hex) else { return false };
    let Ok(sig): Result<[u8; 64], _> = sig.try_into() else { return false };
    vk.verify(message, &Signature::from_bytes(&sig)).is_ok()
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
    };
    Minted { actor, secret_key }
}

/// Mint an agent delegated by `parent`, carrying a chain that roots at a human. `parent` may be a
/// human (the common case) or an already-accountable agent (multi-hop). Returns `None` if `parent`
/// is unaccountable — enforcing "no agent without a human root" at mint.
pub fn mint_agent(handle: &str, parent: &Actor, scope: &str, lifetime: Lifetime) -> Option<Minted> {
    parent.human_principal()?; // parent must resolve to a human, else refuse
    let (id, secret_key) = keypair();
    // Extend the parent's chain (or seed it with the human parent as the root hop) with this agent.
    let mut chain = match &parent.delegation {
        Some(d) => d.chain.clone(),
        None => vec![DelegationHop {
            principal: parent.id.clone(),
            kind: parent.kind,
            scope: "*".to_string(),
            signature: vec![],
        }],
    };
    chain.push(DelegationHop { principal: id.clone(), kind: ActorKind::Agent, scope: scope.to_string(), signature: vec![] });
    let actor = Actor {
        id,
        kind: ActorKind::Agent,
        lifetime,
        handle: handle.to_string(),
        delegation: Some(Delegation { chain }),
        nostr_pubkey: None,
    };
    actor.is_accountable().then_some(Minted { actor, secret_key })
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

    #[test]
    fn agent_chains_to_the_delegating_human() {
        let human = mint_human("justin").actor;
        let agent = mint_agent("agent:reviewer", &human, "issues:*", Lifetime::Ephemeral { expires_unix: 0 })
            .expect("agent delegated by a human is accountable");
        assert!(agent.actor.is_accountable());
        assert_eq!(agent.actor.human_principal(), Some(&human.id));

        // multi-hop: an agent delegating a sub-agent still roots at the human
        let sub = mint_agent("agent:fix", &agent.actor, "issues:close", Lifetime::Static).unwrap();
        assert_eq!(sub.actor.human_principal(), Some(&human.id));
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
        };
        assert!(mint_agent("agent:child", &orphan, "*", Lifetime::Static).is_none());
    }
}
