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
