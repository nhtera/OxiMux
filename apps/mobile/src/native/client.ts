import * as Device from 'expo-device';
import {
  ConnState_Tags,
  MobileClient,
  type ConnState,
  type SessionSummary,
} from 'oximux-core';
import { create } from 'zustand';

import { toArrayBuffer } from './base64';
import { getOrCreateSeed } from './identity';
import { clearHost, endpointIdBytes, loadHost, saveHost } from './hosts';

/**
 * The connection state, flattened for rendering. The Rust core reports a tagged
 * union; the UI only ever needs the tag plus the failure cause, so the tag is
 * widened to a plain string here rather than threading the union through every
 * component.
 */
export type ConnectionPhase =
  | 'idle'
  | 'connecting'
  | 'connected'
  | 'reconnecting'
  | 'disconnected'
  | 'unreachable';

type ClientState = {
  phase: ConnectionPhase;
  /** Populated only in the `unreachable` phase — the last dial's failure. */
  cause?: string;
  sessions: SessionSummary[];
  /** Non-null once a pairing has been attempted; the Rust core owns the connection. */
  client?: MobileClient;
};

type ClientActions = {
  pair: (ticketUrl: string) => Promise<void>;
  /** Reconnect to the stored host. Resolves `false` when none is paired. */
  resume: () => Promise<boolean>;
  refreshSessions: () => Promise<void>;
  disconnect: () => Promise<void>;
  unpair: () => Promise<void>;
};

/**
 * Every branch sets `cause` explicitly — the store merges partial updates, so
 * omitting the key would leave a previous failure's text attached to a later
 * success ("Connected — handshake failed").
 */
function phaseOf(state: ConnState): { phase: ConnectionPhase; cause: string | undefined } {
  switch (state.tag) {
    case ConnState_Tags.Connecting:
      return { phase: 'connecting', cause: undefined };
    case ConnState_Tags.Connected:
      return { phase: 'connected', cause: undefined };
    case ConnState_Tags.Reconnecting:
      return { phase: 'reconnecting', cause: undefined };
    case ConnState_Tags.Disconnected:
      return { phase: 'disconnected', cause: undefined };
    case ConnState_Tags.Unreachable:
      return { phase: 'unreachable', cause: state.inner.cause };
  }
}

/** A stable label the desktop shows in its paired-device list. */
function deviceName(): string {
  return Device.deviceName ?? `${Device.osName ?? 'Mobile'} device`;
}

/**
 * A client bound to this device's persistent identity. The seed is minted and
 * stored here rather than by the core, which has no getter for a seed it
 * generates — see `identity.ts`.
 */
async function newClient(): Promise<MobileClient> {
  return new MobileClient(toArrayBuffer(await getOrCreateSeed()));
}

export const useClient = create<ClientState & ClientActions>((set, get) => ({
  phase: 'idle',
  sessions: [],

  /**
   * Consume a scanned pairing ticket. The seed comes from the keystore so a
   * re-pair against the same desktop reuses this device's existing identity
   * rather than orphaning the record the desktop already holds.
   */
  async pair(ticketUrl: string) {
    const client = await newClient();
    // Register the session-list sink before connecting so the host's initial
    // snapshot and every subsequent change land in the store — the list is
    // push-driven, not polled (the Rust core re-subscribes on each reconnect).
    client.setSessionsSink({ onSessions: (sessions) => set({ sessions }) });
    set({ client, phase: 'connecting', cause: undefined });
    const onState = { onState: (state: ConnState) => set(phaseOf(state)) };

    try {
      await client.connect(ticketUrl, deviceName(), onState);
    } catch (e) {
      // The host refuses `Register` for a device it already knows — an already
      // paired device is expected to `Connect` instead. That is reachable without
      // any user error: on iOS the Keychain survives app deletion while
      // AsyncStorage does not, so a reinstalled app keeps its identity but loses
      // the host record and arrives here trying to pair. Resuming is the correct
      // recovery, and `connect` records the ticket's endpoint id before dialling,
      // so it is available even though the attempt failed. If the device was
      // revoked rather than merely known, the resume fails too and that error —
      // the actionable one — is what surfaces.
      const endpointId = client.hostEndpointId();
      if (!endpointId) throw e;
      await client.reconnect(endpointId, onState);
    }

    // Remember the host only after the handshake lands, so a failed pairing does
    // not leave a record that would send the next launch straight into a resume
    // against a desktop that never accepted this device.
    const endpointId = client.hostEndpointId();
    if (endpointId) await saveHost(new Uint8Array(endpointId));

    // `connect` resolves once the first handshake lands; the driver then keeps
    // the link alive on its own. Seed the list so the first render is populated.
    await get().refreshSessions();
  },

  /**
   * Resume the stored pairing. This is a `Connect`, not a `Register` — the host
   * challenges this device's Ed25519 key, which is why no ticket is needed (and
   * why storing one would not have helped: its secret is single-use).
   */
  async resume() {
    const host = await loadHost();
    if (!host) return false;
    const client = await newClient();
    // See `pair`: the list is kept live by this push sink, not a poll.
    client.setSessionsSink({ onSessions: (sessions) => set({ sessions }) });
    set({ client, phase: 'connecting', cause: undefined });
    await client.reconnect(toArrayBuffer(endpointIdBytes(host)), {
      onState: (state: ConnState) => set(phaseOf(state)),
    });
    await get().refreshSessions();
    return true;
  },

  async refreshSessions() {
    const client = get().client;
    if (!client) return;
    set({ sessions: await client.listSessions() });
  },

  async disconnect() {
    await get().client?.disconnect();
    set({ phase: 'idle', sessions: [], client: undefined, cause: undefined });
  },

  /** Drop the connection and forget the host, sending the app back to pairing. */
  async unpair() {
    await get().disconnect();
    await clearHost();
  },
}));
