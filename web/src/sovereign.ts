// Sovereign (non-custodial) identity crypto — all in the browser. Generate an Ed25519 key here and
// wrap its secret under the user's passphrase (Argon2id + XChaCha20-Poly1305). Hull only ever stores
// the PUBLIC key + the opaque wrapped bundle; it never sees the secret or the passphrase. This is the
// client half of the sovereign-account backend (`/api/auth/sovereign/*`).
import * as ed from "@noble/ed25519";
import { argon2id } from "@noble/hashes/argon2.js";
import { xchacha20poly1305 } from "@noble/ciphers/chacha.js";

// Argon2id params — memory-hard, tuned strong on purpose: the wrapped bundle is fetchable pre-auth
// (for cross-device login), so it can be attacked OFFLINE and the account's security rests entirely
// on this KDF. Stored in the bundle so params can be raised later without breaking existing accounts.
// 64 MiB × 3 passes ≈ a second in-browser — fine for a one-time signup/login step.
const KDF = { m: 65536, t: 3, p: 1, dkLen: 32 } as const;

const hexToBytes = (h: string) => Uint8Array.from((h.match(/../g) ?? []).map((x) => parseInt(x, 16)));
const bytesToHex = (b: Uint8Array) => [...b].map((x) => x.toString(16).padStart(2, "0")).join("");
const b64 = (b: Uint8Array) => btoa(String.fromCharCode(...b));
const unb64 = (s: string) => Uint8Array.from(atob(s), (c) => c.charCodeAt(0));
const rand = (n: number) => crypto.getRandomValues(new Uint8Array(n));
const utf8 = (s: string) => new TextEncoder().encode(s);

export type Identity = { pub: string; secret: string }; // both hex

/** Generate a fresh Ed25519 identity in the browser. The secret never leaves the device unwrapped. */
export async function generateIdentity(): Promise<Identity> {
  const secret = rand(32);
  const pub = await ed.getPublicKeyAsync(secret);
  return { pub: bytesToHex(pub), secret: bytesToHex(secret) };
}

/** Wrap a hex secret under a passphrase → an opaque JSON bundle Hull stores but can never read. */
export function wrapSecret(secretHex: string, passphrase: string): string {
  const salt = rand(16);
  const key = argon2id(utf8(passphrase), salt, KDF);
  const nonce = rand(24); // XChaCha20 24-byte nonce
  const ct = xchacha20poly1305(key, nonce).encrypt(hexToBytes(secretHex));
  return JSON.stringify({ v: 1, kdf: "argon2id", m: KDF.m, t: KDF.t, p: KDF.p, salt: b64(salt), nonce: b64(nonce), ct: b64(ct) });
}

/** Reverse [`wrapSecret`]. Throws if the passphrase is wrong (AEAD tag mismatch) or the bundle is bad. */
export function unwrapSecret(bundle: string, passphrase: string): string {
  const j = JSON.parse(bundle);
  if (j.v !== 1 || j.kdf !== "argon2id") throw new Error("unsupported key bundle");
  const key = argon2id(utf8(passphrase), unb64(j.salt), { m: j.m, t: j.t, p: j.p, dkLen: 32 });
  const pt = xchacha20poly1305(key, unb64(j.nonce)).decrypt(unb64(j.ct));
  return bytesToHex(pt);
}

/** Sign a utf8 message with a hex secret → hex signature (matches the server's `identity::verify`). */
export async function signMessage(secretHex: string, message: string): Promise<string> {
  return bytesToHex(await ed.signAsync(utf8(message), hexToBytes(secretHex)));
}
