import * as Crypto from 'expo-crypto';
import * as SecureStore from 'expo-secure-store';

import { fromBase64, toBase64 } from './base64';

/**
 * The device's Ed25519 app identity seed.
 *
 * This is the private half of the key the desktop pins at pairing time, so it
 * lives in the OS keystore (iOS Keychain / Android Keystore) rather than
 * AsyncStorage. `WHEN_UNLOCKED_THIS_DEVICE_ONLY` is deliberate: the seed must not
 * ride an iCloud backup to another handset, because restoring it elsewhere would
 * silently clone an authorized device — the desktop authorizes *keys*, and has no
 * way to tell two holders of the same key apart.
 */
const SEED_KEY = 'oximux.identity.seed.v1';

const SEED_BYTES = 32;

const OPTIONS: SecureStore.SecureStoreOptions = {
  keychainAccessible: SecureStore.WHEN_UNLOCKED_THIS_DEVICE_ONLY,
};

/**
 * The stored seed, or `undefined` on a device that has never paired. A stored
 * value of the wrong length is treated as absent rather than repaired — passing a
 * short seed to the Rust core would have it silently mint a *fresh* random
 * identity, unpairing the device with no error anyone would see.
 */
export async function loadSeed(): Promise<Uint8Array | undefined> {
  const stored = await SecureStore.getItemAsync(SEED_KEY, OPTIONS);
  if (!stored) return undefined;
  const bytes = fromBase64(stored);
  return bytes.length === SEED_BYTES ? bytes : undefined;
}

/** Persist a seed minted by the Rust core so the pairing survives app restarts. */
export async function saveSeed(seed: Uint8Array): Promise<void> {
  if (seed.length !== SEED_BYTES) {
    throw new Error(`identity seed must be ${SEED_BYTES} bytes, got ${seed.length}`);
  }
  await SecureStore.setItemAsync(SEED_KEY, toBase64(seed), OPTIONS);
}

/**
 * The device's stable identity seed, minting and storing one on first use.
 *
 * The seed is generated *here* rather than by the Rust core on purpose:
 * `MobileClient::new(None)` mints a random identity but exposes no way to read it
 * back, so a client that let Rust generate would get a brand-new key on every
 * launch and silently re-pair — leaving a trail of dead device records on the
 * desktop. Owning generation keeps the keystore the single source of truth.
 */
export async function getOrCreateSeed(): Promise<Uint8Array> {
  const existing = await loadSeed();
  if (existing) return existing;
  const seed = Crypto.getRandomBytes(SEED_BYTES);
  await saveSeed(seed);
  return seed;
}

/** Drop the identity. The desktop keeps its record until the pairing is revoked there. */
export async function clearSeed(): Promise<void> {
  await SecureStore.deleteItemAsync(SEED_KEY, OPTIONS);
}
