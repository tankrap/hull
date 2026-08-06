// WebAuthn browser glue. The server (webauthn-rs) speaks base64url JSON; the browser's
// navigator.credentials API speaks ArrayBuffers. These helpers convert both ways so a passkey
// ceremony is just: fetch options → create/get → post the serialized credential back.

const b64uToBuf = (s: string): ArrayBuffer => {
  s = s.replace(/-/g, "+").replace(/_/g, "/");
  const pad = s.length % 4 ? "=".repeat(4 - (s.length % 4)) : "";
  const bin = atob(s + pad);
  const b = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) b[i] = bin.charCodeAt(i);
  return b.buffer;
};

const bufToB64u = (buf: ArrayBuffer): string => {
  const b = new Uint8Array(buf);
  let s = "";
  for (let i = 0; i < b.length; i++) s += String.fromCharCode(b[i]);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
};

/** Run a registration ceremony against a server `CreationChallengeResponse` (has `.publicKey`). */
export async function createPasskey(options: any) {
  const pk = { ...options.publicKey };
  pk.challenge = b64uToBuf(pk.challenge);
  pk.user = { ...pk.user, id: b64uToBuf(pk.user.id) };
  if (pk.excludeCredentials) pk.excludeCredentials = pk.excludeCredentials.map((c: any) => ({ ...c, id: b64uToBuf(c.id) }));
  const cred = (await navigator.credentials.create({ publicKey: pk })) as PublicKeyCredential;
  const resp = cred.response as AuthenticatorAttestationResponse;
  return {
    id: cred.id,
    rawId: bufToB64u(cred.rawId),
    type: cred.type,
    response: {
      attestationObject: bufToB64u(resp.attestationObject),
      clientDataJSON: bufToB64u(resp.clientDataJSON),
    },
    extensions: cred.getClientExtensionResults(),
  };
}

/** Run an authentication ceremony against a server `RequestChallengeResponse` (has `.publicKey`). */
export async function getPasskey(options: any) {
  const pk = { ...options.publicKey };
  pk.challenge = b64uToBuf(pk.challenge);
  if (pk.allowCredentials) pk.allowCredentials = pk.allowCredentials.map((c: any) => ({ ...c, id: b64uToBuf(c.id) }));
  const cred = (await navigator.credentials.get({ publicKey: pk })) as PublicKeyCredential;
  const resp = cred.response as AuthenticatorAssertionResponse;
  return {
    id: cred.id,
    rawId: bufToB64u(cred.rawId),
    type: cred.type,
    response: {
      authenticatorData: bufToB64u(resp.authenticatorData),
      clientDataJSON: bufToB64u(resp.clientDataJSON),
      signature: bufToB64u(resp.signature),
      userHandle: resp.userHandle ? bufToB64u(resp.userHandle) : null,
    },
    extensions: cred.getClientExtensionResults(),
  };
}
