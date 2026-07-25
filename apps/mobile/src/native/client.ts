import * as Device from 'expo-device';
import {
  ConnState_Tags,
  MobileClient,
  type ConnState,
  type ProjectSummary,
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
  /**
   * The host's projects, as quick-start targets for the grouped session list.
   * Fetched once per connect (they change rarely — a project is added/removed on
   * the desktop, not mid-session), so unlike `sessions` there is no push channel.
   */
  projects: ProjectSummary[];
  /** Non-null once a pairing has been attempted; the Rust core owns the connection. */
  client?: MobileClient;
};

/**
 * What `pair` is doing right now, for a screen that would otherwise show nothing
 * between the scan and the result.
 *
 * These are steps in one user-visible act, not transport states — `connecting`
 * covers dialling *and* registering, because a phone owner has no use for the
 * distinction. `resuming` is the one branch worth naming: it means the host knew
 * this device already, so the attempt became a reconnect and pays a second dial.
 * Without it that doubled wait is indistinguishable from a hang.
 */
export type PairStep = 'connecting' | 'resuming';

type ClientActions = {
  pair: (ticketUrl: string, onStep?: (step: PairStep) => void) => Promise<void>;
  /** Reconnect to the stored host. Resolves `false` when none is paired. */
  resume: () => Promise<boolean>;
  /**
   * Stand the host link back up if it is not already live. Idempotent: a no-op
   * while `connected`/`connecting`/`reconnecting`, and a fresh `resume` otherwise
   * (`idle`/`disconnected`/`unreachable` — the reconnect driver has stopped and
   * nothing else will redial). Safe to call on every app foreground and on
   * pull-to-refresh.
   */
  ensureConnected: () => Promise<void>;
  refreshSessions: () => Promise<void>;
  /** Re-fetch the host's projects. Best-effort — a failure leaves the list as-is. */
  refreshProjects: () => Promise<void>;
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

/**
 * The in-flight `resume`, so a cold-boot resume and a foreground `ensureConnected`
 * that race at startup share one dial rather than each minting a client (the later
 * one would supersede the earlier, tearing down a connection that was about to
 * land). Cleared when the dial settles — success or failure — so a later
 * reconnect starts fresh. Module-scoped rather than in the store because it is
 * plumbing, not rendered state.
 */
let dialing: Promise<boolean> | null = null;

export const useClient = create<ClientState & ClientActions>((set, get) => ({
  phase: 'idle',
  sessions: [],
  projects: [],

  /**
   * Consume a scanned pairing ticket. The seed comes from the keystore so a
   * re-pair against the same desktop reuses this device's existing identity
   * rather than orphaning the record the desktop already holds.
   */
  async pair(ticketUrl: string, onStep?: (step: PairStep) => void) {
    onStep?.('connecting');
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
      // Say so rather than silently paying a second dial: from the outside this
      // is the slowest path pairing has, and an unexplained double wait is what
      // a hang looks like.
      onStep?.('resuming');
      await client.reconnect(endpointId, onState);
    }

    // Remember the host only after the handshake lands, so a failed pairing does
    // not leave a record that would send the next launch straight into a resume
    // against a desktop that never accepted this device.
    const endpointId = client.hostEndpointId();
    if (endpointId) await saveHost(new Uint8Array(endpointId));

    // `connect` resolves once the first handshake lands; the driver then keeps
    // the link alive on its own. Seed the lists so the first render is populated.
    await get().refreshSessions();
    await get().refreshProjects();
  },

  /**
   * Resume the stored pairing. This is a `Connect`, not a `Register` — the host
   * challenges this device's Ed25519 key, which is why no ticket is needed (and
   * why storing one would not have helped: its secret is single-use).
   */
  async resume() {
    // Coalesce concurrent calls (cold-boot boot effect + foreground reconnect)
    // onto one dial; whoever loses the race awaits the same outcome.
    if (dialing) return dialing;
    dialing = (async () => {
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
      await get().refreshProjects();
      return true;
    })();
    try {
      return await dialing;
    } finally {
      dialing = null;
    }
  },

  async ensureConnected() {
    const phase = get().phase;
    // Already live, or a dial is already in flight — leave it be.
    if (phase === 'connected' || phase === 'connecting' || phase === 'reconnecting') return;
    // idle / disconnected / unreachable: the reconnect driver has given up (or
    // never started — a JS reload can restore a screen with the store reset), so
    // stand a fresh one up. A dial failure re-surfaces through `phase`, so there
    // is nothing to propagate to a foreground tick or a pull-to-refresh.
    try {
      await get().resume();
    } catch {
      // phase already carries the failure (unreachable)
    }
  },

  async refreshSessions() {
    const client = get().client;
    if (!client) return;
    set({ sessions: await client.listSessions() });
  },

  async refreshProjects() {
    const client = get().client;
    if (!client) return;
    try {
      set({ projects: await client.listProjects() });
    } catch {
      // A host that predates the project RPC (or refuses it) simply leaves the
      // grouped list falling back to a flat one — not worth surfacing.
    }
  },

  async disconnect() {
    await get().client?.disconnect();
    set({ phase: 'idle', sessions: [], projects: [], client: undefined, cause: undefined });
  },

  /**
   * Drop the connection and forget the host, sending the app back to pairing.
   *
   * Tells the desktop first, so its paired-devices list loses this phone instead
   * of keeping a row for a device that has gone. Best-effort on purpose: a
   * desktop that is asleep, unreachable, or too old to know the call must not be
   * able to keep this phone enrolled, so the local forget happens either way and
   * the failure costs only a stale row the desktop's own Forget clears.
   */
  async unpair() {
    try {
      await get().client?.unpair();
    } catch {
      // Left enrolled on the desktop; the phone still forgets it here.
    }
    await get().disconnect();
    await clearHost();
  },
}));
