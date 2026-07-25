// The Rust core exchanges raw bytes, but both storage backends (SecureStore and
// AsyncStorage) hold strings only — so byte payloads round-trip through base64.

export function toBase64(bytes: Uint8Array): string {
  let binary = '';
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return globalThis.btoa(binary);
}

export function fromBase64(encoded: string): Uint8Array {
  const binary = globalThis.atob(encoded);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

/**
 * Copy a view's own bytes into a standalone ArrayBuffer for the FFI.
 *
 * Slicing by `byteOffset`/`byteLength` rather than passing `.buffer` matters: on
 * a pooled or subarray-backed `Uint8Array` those differ, and the core would read
 * the wrong bytes — or the wrong length — as key material.
 */
export function toArrayBuffer(bytes: Uint8Array): ArrayBuffer {
  return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) as ArrayBuffer;
}
