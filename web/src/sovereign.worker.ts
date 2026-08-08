// Runs the memory-hard Argon2id wrap/unwrap off the main thread so signup and login don't freeze the
// UI for ~1s. Pure crypto — it just calls the same sync wrapSecret/unwrapSecret, so results are
// identical to running them inline. A wrong passphrase surfaces as a rejected message (unwrap throws).
import { wrapSecret, unwrapSecret } from "./sovereign";

self.onmessage = (e: MessageEvent) => {
  const { id, op, a, b } = e.data as { id: number; op: "wrap" | "unwrap"; a: string; b: string };
  try {
    const result = op === "wrap" ? wrapSecret(a, b) : unwrapSecret(a, b);
    (self as unknown as Worker).postMessage({ id, ok: true, result });
  } catch (err) {
    (self as unknown as Worker).postMessage({ id, ok: false, error: String((err as Error)?.message || err) });
  }
};
