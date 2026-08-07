//! Passkey (WebAuthn) accounts — the server side of passwordless login.
//!
//! Hull's "hosted accounts" model: a [`hull_core::User`] logs in with a passkey (and email/username),
//! and Hull holds that user's human signing key server-side so it can sign agent delegations for them
//! (a passkey can only sign server challenges, never an arbitrary delegation message). This module
//! owns the WebAuthn relying-party config and the short-lived ceremony state; the HTTP handlers and
//! durable credential storage live in `lib.rs` / the domain store.

use webauthn_rs::prelude::*;

/// Build the relying party from env (localhost defaults for dev). `rp_id` is the effective domain
/// (no scheme/port); `origin` must match the browser's origin exactly, including port.
pub fn build() -> Webauthn {
    let rp_id = std::env::var("HULL_WEBAUTHN_RP_ID").unwrap_or_else(|_| "localhost".into());
    let origin = std::env::var("HULL_WEBAUTHN_ORIGIN").unwrap_or_else(|_| "http://localhost:5930".into());
    let rp_origin = Url::parse(&origin).expect("HULL_WEBAUTHN_ORIGIN must be a URL");
    WebauthnBuilder::new(&rp_id, &rp_origin)
        .expect("invalid WebAuthn relying-party config")
        .rp_name("hull")
        .build()
        .expect("failed to build WebAuthn")
}

/// In-flight signup registration (new user): the pending identity + the WebAuthn registration state.
pub struct RegFlow {
    pub username: String,
    pub email: String,
    pub uuid: Uuid,
    pub state: PasskeyRegistration,
}

/// In-flight "add a passkey to an existing account" registration.
pub struct AddFlow {
    pub user_id: String,
    pub state: PasskeyRegistration,
}

/// In-flight authentication: which user is proving possession + the challenge state.
pub struct AuthFlow {
    pub user_id: String,
    pub state: PasskeyAuthentication,
}
