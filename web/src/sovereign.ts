// Sovereign (non-custodial) identity crypto — all in the browser. Generate an Ed25519 key here and
// wrap its secret under the user's passphrase (Argon2id + XChaCha20-Poly1305). Hull only ever stores
// the PUBLIC key + the opaque wrapped bundle; it never sees the secret or the passphrase. This is the
// client half of the sovereign-account backend (`/api/auth/sovereign/*`).
import * as ed from "@noble/ed25519";
import { argon2id } from "@noble/hashes/argon2.js";
import { sha256 } from "@noble/hashes/sha2.js";
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

// ── off-main-thread KDF ─────────────────────────────────────────────────────────────────────────
// The Argon2id step takes ~1s and would jank the UI if run inline. wrapSecretAsync/unwrapSecretAsync
// offload it to a worker; if Workers are unavailable (or the worker fails to load) they fall back to
// the sync path, which still works — it just blocks. Results are byte-identical either way.
let _worker: Worker | null = null;
let _seq = 0;
const _pending = new Map<number, { resolve: (v: string) => void; reject: (e: unknown) => void }>();
function kdfWorker(): Worker | null {
  if (typeof Worker === "undefined") return null;
  if (_worker) return _worker;
  try {
    const w = new Worker(new URL("./sovereign.worker.ts", import.meta.url), { type: "module" });
    w.onmessage = (e: MessageEvent) => {
      const { id, ok, result, error } = e.data as { id: number; ok: boolean; result?: string; error?: string };
      const p = _pending.get(id);
      if (!p) return;
      _pending.delete(id);
      ok ? p.resolve(result as string) : p.reject(new Error(error));
    };
    w.onerror = () => {
      // The worker itself failed to run — reject anything in flight and drop it so the next call falls
      // back to the sync path instead of hanging forever on a dead worker.
      for (const { reject } of _pending.values()) reject(new Error("kdf worker error"));
      _pending.clear();
      _worker = null;
    };
    _worker = w;
    return w;
  } catch {
    return null;
  }
}
function runOnWorker(op: "wrap" | "unwrap", a: string, b: string): Promise<string> {
  const w = kdfWorker();
  if (!w) return Promise.resolve(op === "wrap" ? wrapSecret(a, b) : unwrapSecret(a, b));
  return new Promise((resolve, reject) => {
    const id = ++_seq;
    _pending.set(id, { resolve, reject });
    w.postMessage({ id, op, a, b });
  });
}
/** [`wrapSecret`] off the main thread (falls back to sync if Workers are unavailable). */
export const wrapSecretAsync = (secretHex: string, passphrase: string) => runOnWorker("wrap", secretHex, passphrase);
/** [`unwrapSecret`] off the main thread. Rejects on a wrong passphrase, same as the sync version throws. */
export const unwrapSecretAsync = (bundle: string, passphrase: string) => runOnWorker("unwrap", bundle, passphrase);

/** A rough passphrase-strength estimate for the signup meter. Dependency-free: entropy = length ×
 *  log2(character-pool). NOT a dictionary check — it can't tell that "password1234" is weak — so it's
 *  a guide, not a gate. The real protection is the memory-hard KDF plus a long passphrase. */
export function passphraseStrength(pass: string): { score: 0 | 1 | 2 | 3 | 4; label: string } {
  if (!pass) return { score: 0, label: "" };
  const pool =
    (/[a-z]/.test(pass) ? 26 : 0) +
    (/[A-Z]/.test(pass) ? 26 : 0) +
    (/[0-9]/.test(pass) ? 10 : 0) +
    (/[^a-zA-Z0-9]/.test(pass) ? 32 : 0);
  const bits = pass.length * Math.log2(Math.max(pool, 1));
  const score = bits < 40 ? 1 : bits < 60 ? 2 : bits < 80 ? 3 : 4;
  return { score, label: ["", "weak", "fair", "good", "strong"][score] };
}

/** Sign a utf8 message with a hex secret → hex signature (matches the server's `identity::verify`). */
export async function signMessage(secretHex: string, message: string): Promise<string> {
  return bytesToHex(await ed.signAsync(utf8(message), hexToBytes(secretHex)));
}

// Provenance: a sovereign author attests, client-side, that they authored a landed change. The server
// only stores + relays it (it can't sign — the key is here). Mirror of hull-server's ProvenanceClaim.
export type ProvenanceClaim = { v: number; change: string; actor: string; repo: string; intent: string; ts: number };
export type SignedProvenance = { claim: ProvenanceClaim; ed_sig: string };

/** The exact bytes the actor signs — must byte-match Rust `ProvenanceClaim::signing_bytes`: a flat,
 *  domain-separated form (NOT JSON, which serde and JSON.stringify serialize differently), with the
 *  free-text intent folded to its SHA-256 so newlines/unicode can't break the line structure. */
export function provenanceSigningBytes(c: ProvenanceClaim): string {
  const intentSha = bytesToHex(sha256(utf8(c.intent)));
  return `hull-provenance:v1\nchange=${c.change}\nactor=${c.actor}\nrepo=${c.repo}\nintent_sha256=${intentSha}\nts=${c.ts}`;
}

/** Sign a provenance claim with a hex secret → the SignedProvenance bundle the server stores + embeds. */
export async function signProvenance(secretHex: string, claim: ProvenanceClaim): Promise<SignedProvenance> {
  return { claim, ed_sig: await signMessage(secretHex, provenanceSigningBytes(claim)) };
}
