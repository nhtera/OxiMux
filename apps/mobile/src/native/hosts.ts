import AsyncStorage from '@react-native-async-storage/async-storage';

import { fromBase64, toBase64 } from './base64';

/**
 * The desktop this device is paired with.
 *
 * Only the host's endpoint id is kept — it is the host's *public* key and the
 * routing address, so plain AsyncStorage is the right home for it. The pairing
 * ticket is deliberately NOT stored: its `handshake_secret` is single-use and the
 * host rotates it, so a replayed ticket would fail. Reconnecting instead proves
 * this device's own Ed25519 identity, which lives in the keystore.
 */
const HOST_KEY = 'oximux.host.v1';

const ENDPOINT_ID_BYTES = 32;

export type PairedHost = {
  /** base64 of the host's 32-byte endpoint id. */
  endpointId: string;
  /** Shown while reconnecting; purely cosmetic. */
  label?: string;
  pairedAt: number;
};

/**
 * The stored host, or `undefined` if this device has never paired. A record whose
 * id is the wrong length is treated as absent rather than repaired — handing a
 * malformed id to the core would fail the dial with a confusing transport error
 * instead of simply sending the user back to the pair screen.
 */
export async function loadHost(): Promise<PairedHost | undefined> {
  const raw = await AsyncStorage.getItem(HOST_KEY);
  if (!raw) return undefined;
  try {
    const host = JSON.parse(raw) as PairedHost;
    return fromBase64(host.endpointId).length === ENDPOINT_ID_BYTES ? host : undefined;
  } catch {
    return undefined;
  }
}

export async function saveHost(endpointId: Uint8Array, label?: string): Promise<void> {
  if (endpointId.length !== ENDPOINT_ID_BYTES) {
    throw new Error(`endpoint id must be ${ENDPOINT_ID_BYTES} bytes, got ${endpointId.length}`);
  }
  const host: PairedHost = { endpointId: toBase64(endpointId), label, pairedAt: Date.now() };
  await AsyncStorage.setItem(HOST_KEY, JSON.stringify(host));
}

/**
 * Forget the host. The identity seed is left alone on purpose: re-pairing with
 * the same desktop should reuse this device's key so the desktop updates its
 * existing record instead of accumulating orphans.
 */
export async function clearHost(): Promise<void> {
  await AsyncStorage.removeItem(HOST_KEY);
}

/** The stored id as bytes, ready for `MobileClient.reconnect`. */
export function endpointIdBytes(host: PairedHost): Uint8Array {
  return fromBase64(host.endpointId);
}
