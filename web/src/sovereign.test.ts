import { describe, it, expect } from "vitest";
import { provenanceSigningBytes, passphraseStrength } from "./sovereign";

// Pins the provenance signing bytes to the SAME literal the server pins in
// crates/hull-server/src/nostr.rs (provenance_signing_bytes_are_pinned). Both sides must agree exactly
// or a sovereign author's client signature won't verify server-side. If either side changes the format,
// one of these two tests fails instead of shipping silently-broken signatures.
describe("provenanceSigningBytes", () => {
  it("byte-matches the server's ProvenanceClaim::signing_bytes", () => {
    const bytes = provenanceSigningBytes({ v: 1, change: "blake3:c1", actor: "abcd", repo: "acme/web", intent: "hello", ts: 1_700_000_000 });
    // sha256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
    expect(bytes).toBe(
      "hull-provenance:v1\nchange=blake3:c1\nactor=abcd\nrepo=acme/web\nintent_sha256=2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824\nts=1700000000",
    );
  });
});

describe("passphraseStrength", () => {
  it("scores empty, short, and long passphrases sensibly", () => {
    expect(passphraseStrength("").score).toBe(0);
    // short, single class → weak
    expect(passphraseStrength("aaaaa").score).toBe(1);
    // a long, mixed passphrase → strong; and it always rises (never falls) as length grows
    expect(passphraseStrength("Tr0ub4dour&3xtra-Long-Phrase").score).toBe(4);
    const short = passphraseStrength("Ab1!xy");
    const long = passphraseStrength("Ab1!xy".repeat(6));
    expect(long.score).toBeGreaterThanOrEqual(short.score);
  });
});
