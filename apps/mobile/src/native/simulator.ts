import { SimSubscribeOutcome_Tags, type SimSubscribeOutcome, type SimViewable } from 'oximux-core';
import { useCallback, useEffect, useRef, useState } from 'react';

import { toBase64 } from './base64';
import { useClient } from './client';
import { describeError } from './errors';

/** A view-stream frame's kind, as the core passes it. */
export const FRAME_CONFIG = 0;
export const FRAME_KEY = 1;
export const FRAME_DELTA = 2;

/** One frame for the page: its kind and its bytes, base64. */
export type ViewFrame = { k: number; b: string };

/** Where frames go once the page is up. */
export type ViewWriter = (frame: ViewFrame) => void;

/** How often a pending subscribe asks again while the desktop's user decides. */
export const PENDING_RETRY_MS = 1000;
/** How soon a view that ended (a reconnect, the device restarting) subscribes again. */
export const RESUBSCRIBE_MS = 1500;
/** How often the Relayed badge is refreshed. */
const RELAYED_POLL_MS = 3000;
/** The soonest a page that failed to decode starts the view over again. */
export const RESTART_MIN_MS = 3000;

/** Where the view stands, for the screen. */
export type ViewPhase =
  /** Asking the desktop. */
  | { kind: 'starting' }
  /** The desktop asked its user; asking again shortly. */
  | { kind: 'waiting'; message: string }
  | { kind: 'live'; width: number; height: number }
  | { kind: 'denied'; message: string; retryMs: number }
  | { kind: 'refused'; message: string }
  /** Off screen or in the background: not watching. */
  | { kind: 'paused' }
  | { kind: 'error'; message: string };

/** What a subscribe's outcome means for the screen, and its token if live. */
export function phaseOf(outcome: SimSubscribeOutcome): { phase: ViewPhase; token?: string } {
  switch (outcome.tag) {
    case SimSubscribeOutcome_Tags.Started:
      return { phase: { kind: 'live', width: outcome.inner.width, height: outcome.inner.height }, token: outcome.inner.token };
    case SimSubscribeOutcome_Tags.Pending:
      return { phase: { kind: 'waiting', message: 'Waiting for the desktop to allow this phone to watch…' } };
    case SimSubscribeOutcome_Tags.Denied: {
      const secs = Number(outcome.inner.retryAfterSecs);
      return {
        phase: {
          kind: 'denied',
          message: `The desktop did not allow watching this device. You can ask again in ${Math.max(1, Math.ceil(secs))} s.`,
          retryMs: secs * 1000,
        },
      };
    }
    case SimSubscribeOutcome_Tags.Refused:
      return { phase: { kind: 'refused', message: outcome.inner.message } };
  }
}

/**
 * Frames held while no writer is registered (the page is still loading),
 * kept decodable: the last config, then the last key frame and the deltas
 * after it. A delta is useless without every frame before it back to its
 * key frame, so a buffer that overflows drops the whole run and takes
 * nothing more until the next key frame. (Should none come before the page
 * is up, the page sees a delta it cannot use and the view starts over.)
 */
export class ViewBuffer {
  private config: ViewFrame | null = null;
  private run: ViewFrame[] = [];
  constructor(private readonly limit = 90) {}

  push(frame: ViewFrame) {
    if (frame.k === FRAME_CONFIG) {
      this.config = frame;
      this.run = [];
    } else if (frame.k === FRAME_KEY) {
      this.run = [frame];
    } else if (this.run.length >= this.limit) {
      // Too far behind: the run is broken.
      this.run = [];
    } else if (this.run.length > 0) {
      this.run.push(frame);
    }
  }

  /** Everything decodable, in order, and empty again. */
  drain(): ViewFrame[] {
    const out = this.config ? [this.config, ...this.run] : [...this.run];
    this.run = [];
    // The config stays: a later page reload needs it before any key frame.
    return out;
  }
}

/**
 * Frames for tokens not known yet. The desktop starts a view's stream as it
 * answers the subscribe, and the two travel separately, so the first frames
 * (the config among them), or even the stream's end, can arrive before the
 * answer that names their token. They are held while a subscribe is in
 * flight, and handed over when its answer comes.
 */
export class EarlyFrames {
  private held = new Map<string, { buffer: ViewBuffer; ended: boolean }>();
  constructor(private readonly tokens = 4) {}

  private entry(token: string) {
    let entry = this.held.get(token);
    if (!entry) {
      // Only a few: the oldest unknown token goes first.
      if (this.held.size >= this.tokens) this.held.delete(this.held.keys().next().value as string);
      entry = { buffer: new ViewBuffer(), ended: false };
      this.held.set(token, entry);
    }
    return entry;
  }

  push(token: string, frame: ViewFrame) {
    this.entry(token).buffer.push(frame);
  }

  end(token: string) {
    this.entry(token).ended = true;
  }

  /** What arrived for `token`, and forget every other. */
  take(token: string): { buffer: ViewBuffer; ended: boolean } | undefined {
    const entry = this.held.get(token);
    this.held.clear();
    return entry;
  }

  clear() {
    this.held.clear();
  }
}

/** The devices this phone may watch. */
export function useSimViewables(): {
  devices: SimViewable[];
  refused?: string;
  loading: boolean;
  error?: string;
  reload: () => void;
} {
  const client = useClient((s) => s.client);
  const [devices, setDevices] = useState<SimViewable[]>([]);
  const [refused, setRefused] = useState<string>();
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string>();
  const [nonce, setNonce] = useState(0);

  useEffect(() => {
    if (!client) return;
    let live = true;
    client
      .listSimViewables()
      .then((list) => {
        if (!live) return;
        setDevices(list.devices);
        setRefused(list.refused);
        setError(undefined);
        setLoading(false);
      })
      .catch((e: unknown) => {
        if (!live) return;
        setError(describeError(e));
        setLoading(false);
      });
    return () => {
      live = false;
    };
  }, [client, nonce]);

  const reload = () => {
    setLoading(true);
    setNonce((n) => n + 1);
  };
  return { devices, refused, loading, error, reload };
}

/**
 * Watch one device while `active` (the screen focused and the app in front).
 *
 * The picture never passes through React state: frames go straight to the
 * page's writer (or the buffer until it registers), as terminal output does.
 * Going inactive unsubscribes at once, so a phone in a pocket stops costing
 * the desktop anything and its indicator goes away; coming back subscribes
 * again, which the desktop does not ask about twice in one run.
 */
export function useSimView(
  device: string,
  active: boolean
): {
  phase: ViewPhase;
  relayed?: boolean;
  subscribe: (writer: ViewWriter) => () => void;
  retry: () => void;
  /** The page cannot decode: start over (at most every few seconds). */
  restart: () => void;
} {
  const client = useClient((s) => s.client);
  const [phase, setPhase] = useState<ViewPhase>({ kind: 'starting' });
  const [relayed, setRelayed] = useState<boolean>();
  const [nonce, setNonce] = useState(0);
  const token = useRef<string | null>(null);
  const writer = useRef<ViewWriter | null>(null);
  const buffer = useRef(new ViewBuffer());
  const early = useRef(new EarlyFrames());
  /** Subscribes awaiting their answer. */
  const asking = useRef(0);
  const lastRestart = useRef(0);

  useEffect(() => {
    if (!client || !active) return;
    let live = true;
    const pending = early.current;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const later = (ms: number) => {
      timer = setTimeout(() => void start(), ms);
    };

    // To the page, or held until it is up. The config is kept either way:
    // a page that reloads needs it again before any key frame decodes.
    const deliver = (frame: ViewFrame) => {
      const w = writer.current;
      if (!w) {
        buffer.current.push(frame);
        return;
      }
      if (frame.k === FRAME_CONFIG) buffer.current.push(frame);
      w(frame);
    };

    client.setSimViewSink({
      onFrame: (t: string, kind: number, _pts: bigint, data: ArrayBuffer) => {
        if (!live) return;
        // One sink serves every view: frames for another token are not
        // ours, unless they are for the one a subscribe is about to name.
        if (t !== token.current) {
          if (asking.current > 0) pending.push(t, { k: kind, b: toBase64(new Uint8Array(data)) });
          return;
        }
        deliver({ k: kind, b: toBase64(new Uint8Array(data)) });
      },
      onEnd: (t: string) => {
        if (!live) return;
        if (t !== token.current) {
          if (asking.current > 0) pending.end(t);
          return;
        }
        ended();
      },
    });

    function ended() {
      token.current = null;
      setPhase({ kind: 'starting' });
      later(RESUBSCRIBE_MS);
    }

    async function start() {
      if (!live || !client) return;
      asking.current += 1;
      try {
        const { phase: next, token: started } = phaseOf(await client.subscribeSimView(device));
        asking.current -= 1;
        if (!live) {
          if (started) void client.unsubscribeSimView(started).catch(() => {});
          return;
        }
        const held = started ? pending.take(started) : undefined;
        token.current = started ?? null;
        setPhase(next);
        setRelayed(client.connectionRelayed() ?? undefined);
        if (held) {
          if (writer.current) for (const frame of held.buffer.drain()) deliver(frame);
          else buffer.current = held.buffer;
          if (held.ended) ended();
        }
        if (next.kind === 'waiting') later(PENDING_RETRY_MS);
        // A Deny is the desktop user's answer: asking again is the phone
        // user's choice (Try again), never automatic.
      } catch (e) {
        asking.current -= 1;
        if (!live) return;
        setPhase({ kind: 'error', message: describeError(e) });
        later(RESUBSCRIBE_MS * 2);
      }
    }
    void start();

    const poll = setInterval(() => setRelayed(client.connectionRelayed() ?? undefined), RELAYED_POLL_MS);

    return () => {
      live = false;
      // Coming back starts over.
      setPhase({ kind: 'starting' });
      if (timer) clearTimeout(timer);
      clearInterval(poll);
      const held = token.current;
      token.current = null;
      buffer.current = new ViewBuffer();
      pending.clear();
      if (held) void client.unsubscribeSimView(held).catch(() => {});
    };
  }, [client, device, active, nonce]);

  const subscribe = useCallback((next: ViewWriter) => {
    writer.current = next;
    for (const frame of buffer.current.drain()) next(frame);
    return () => {
      if (writer.current === next) writer.current = null;
    };
  }, []);

  // The page cannot go on (its decoder failed, or fell behind): start the
  // view over, which brings a fresh config and key frame.
  const restart = useCallback(() => {
    const now = Date.now();
    if (now - lastRestart.current < RESTART_MIN_MS) return;
    lastRestart.current = now;
    setNonce((n) => n + 1);
  }, []);

  return { phase: active ? phase : PAUSED, relayed, subscribe, retry: () => setNonce((n) => n + 1), restart };
}

const PAUSED: ViewPhase = { kind: 'paused' };
