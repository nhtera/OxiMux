/**
 * Render an error from the Rust core as something a human can act on.
 *
 * The generated `MobileError` variants are tagged-enum classes that also extend
 * `Error`, so an `instanceof Error` check matches first and yields a bare
 * "MobileError.Transport" with the actual cause dropped. The real detail lives in
 * `inner[0]`, so the tag check has to come first.
 */
export function describeError(error: unknown): string {
  if (error && typeof error === 'object' && 'tag' in error) {
    const tag = String((error as { tag: unknown }).tag);
    const inner = (error as { inner?: readonly unknown[] }).inner;
    const detail = Array.isArray(inner) && inner.length > 0 ? String(inner[0]) : undefined;
    return detail ? `${tag}: ${detail}` : tag;
  }
  return error instanceof Error ? error.message : String(error);
}

/**
 * Whether a failure means the host has no such session — the desktop closed the
 * tab, so nothing about this session id will work again.
 *
 * Matched on the rendered text rather than a variant, because the wire's
 * `RpcError` is flattened to a string well before it reaches here: the core
 * carries it as `Rpc(String)`, so the discriminated shape is already gone. The
 * needle is the wire enum's own variant name, which is as stable as the protocol.
 */
export function isUnknownSession(error: unknown): boolean {
  return describeError(error).includes('UnknownSession');
}

/**
 * A pairing failure as a sentence with a next step, rather than the raw tag.
 *
 * Matched on substrings of the rendered error, not on the enum: the refusal
 * reaches here as `Transport(cause)` carrying the reconnect driver's message, so
 * the tag alone can't tell a refused code from an unreachable Mac. That makes
 * this best-effort by construction — anything unrecognised keeps the technical
 * text, which is more use than a wrong guess dressed up as a diagnosis.
 *
 * The refusal case deliberately does not claim *which* refusal it was. The host
 * answers expired, already-used, revoked, and already-known identically (it
 * declines to tell an unauthorized caller which), so naming one would be a
 * guess. Listing the two the user can act on is honest and still actionable.
 */
export function describePairingError(error: unknown): string {
  const raw = describeError(error).toLowerCase();

  if (raw.includes('unauthorized') || raw.includes('pairing failed')) {
    return 'Your desktop refused this code. It may have expired or already been used — open Settings → Remote there and tap "Pair a device" for a fresh one.';
  }
  if (raw.includes('badticket') || raw.includes('invalid') || raw.includes('not an oximux')) {
    return "That isn't a valid OxiMux pairing link.";
  }
  if (raw.includes('timed out') || raw.includes('timeout')) {
    return "Couldn't reach your Mac in time. Check it's awake and on a network, then try again.";
  }
  if (raw.includes('unreachable') || raw.includes('transport') || raw.includes('connection')) {
    return "Couldn't reach your Mac. Check that it's awake and that remote access is still on.";
  }
  return describeError(error);
}
