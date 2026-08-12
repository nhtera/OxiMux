import type { TerminalInfo } from 'oximux-core';
import { useCallback, useEffect, useRef, useState } from 'react';

import { toArrayBuffer, toBase64 } from './base64';
import { useClient } from './client';
import { describeError } from './errors';

/** What the screen needs to render one attached terminal. */
export type TerminalView = {
  /**
   * Register the emulator's writer. Buffered frames are drained into it
   * immediately, then frames stream straight through. Returns an unsubscribe.
   */
  subscribe: (writer: FrameWriter) => () => void;
  error?: string;
  /** True until the first attach resolves. */
  loading: boolean;
  send: (data: string) => void;
  resize: (cols: number, rows: number) => void;
  /** Re-attach and replace the screen — the recovery from a gap. */
  resync: () => void;
  dismissError: () => void;
};

/** One instruction for the emulator, in arrival order. */
export type Frame =
  | { kind: 'write'; base64: string }
  /** Replace the screen wholesale: a fresh snapshot after attach or a gap. */
  | { kind: 'reset'; base64: string; cols: number; rows: number };

/** Where frames go once the emulator is up. */
export type FrameWriter = (frame: Frame) => void;

/**
 * How many frames to hold while no writer is registered — the window between
 * `attach` resolving and the WebView reporting ready.
 *
 * Bounded because a terminal that is producing output faster than the page can
 * boot must not grow this array without limit. Overflowing is not a data loss
 * that has to be papered over: the buffer is dropped and the screen re-attaches
 * for a fresh snapshot, which is the same recovery a gap already uses.
 */
const PREROLL_LIMIT = 256;

/**
 * In-flight `attach` per PTY, so a screen being torn down can detach *behind*
 * the attach of the screen replacing it.
 *
 * Module-level rather than a ref because the two sides of that race are two
 * different hook instances — a ref would give each of them its own view and
 * neither would see the other's work.
 */
const attachInFlight = new Map<string, Promise<void>>();

/**
 * Attach to one terminal and expose its byte stream.
 *
 * Deliberately not stateful beyond a pre-roll buffer: the emulator lives in the
 * WebView and owns the screen, so keeping a second representation here would be
 * a parallel model to drift out of sync with the one the user is looking at.
 *
 * **Output never passes through React state.** Frames are pushed straight to the
 * registered writer. Holding them in state instead would re-render the screen
 * once per chunk and copy an ever-growing array to do it, so a busy terminal
 * (a build, an install) degrades the longer it runs — and nothing ever trims the
 * array, because the WebView has already consumed it.
 *
 * The gap signal is honoured rather than logged. It travels the whole way from
 * the relay daemon's dropped write, through the host and the FFI, to here — and
 * the only correct response is to re-attach and replace the screen, because a
 * gapped terminal is rendering a hole it cannot detect on its own.
 */
export function useTerminal(ptyId: string): TerminalView {
  const client = useClient((s) => s.client);
  const [error, setError] = useState<string>();
  const [loading, setLoading] = useState(true);
  const clientRef = useRef(client);
  clientRef.current = client;

  /**
   * False once this screen has been torn down.
   *
   * iOS does not always reap a WebView's content process when the view
   * unmounts, and a lingering page keeps its emulator — and therefore its
   * `onData` handler — alive. Every stale instance then forwards the same
   * keystroke, so a character typed after N visits reaches the host N times.
   * The sink is already guarded; this guards the other direction, so a view
   * that is no longer on screen cannot type into the user's shell.
   */
  const alive = useRef(true);
  /** Whether an attachment for this PTY is currently held on the host. */
  const held = useRef(false);
  /** Tail of the outbound keystroke chain — see `send`. */
  const sendChain = useRef<Promise<unknown>>(Promise.resolve());
  const writerRef = useRef<FrameWriter | null>(null);
  const preroll = useRef<Frame[]>([]);
  /** Set when the pre-roll overflowed, so the next writer resyncs instead of
   *  rendering a screen with a hole in it. */
  const dropped = useRef(false);
  /** Whether a writer has already consumed frames. A second one is a reloaded
   *  page looking at a blank emulator, which no buffer can refill. */
  const served = useRef(false);

  const emit = useCallback((frame: Frame) => {
    const writer = writerRef.current;
    if (writer) {
      writer(frame);
      return;
    }
    if (preroll.current.length >= PREROLL_LIMIT) {
      preroll.current = [];
      dropped.current = true;
      return;
    }
    preroll.current.push(frame);
  }, []);

  const attach = useCallback(async () => {
    const current = clientRef.current;
    if (!current) {
      setError('Not connected to the desktop.');
      setLoading(false);
      return;
    }
    // Serialize with any attach already in flight for this PTY.
    //
    // Two attaches routinely start at once — the effect's initial one and the
    // re-attach a reloaded page triggers — and both would read `held` as false
    // before either had set it, so the release-before-attach below would not
    // fire and the host would end up holding two attachments for one screen.
    // It fans output out once per attachment, so that alone doubles every byte
    // the user sees, for as long as the screen is open.
    const inFlight = attachInFlight.get(ptyId);
    if (inFlight) await inFlight.catch(() => {});

    const run = (async () => {
      try {
        // Release the attachment we already hold before taking another.
        //
        // Attaching is not idempotent: each one is a separate attachment on the
        // host, and the host fans output out once per attachment. Re-attaching
        // to recover a reloaded page (or a gap) without letting go first
        // therefore leaves the previous one live, and every byte comes back one
        // extra time for the rest of the session. The host retires an orphan
        // eventually, but only when its next output wakes it — too late to stop
        // the duplicate the user is already reading.
        if (held.current) {
          held.current = false;
          await current.detachTerminal(ptyId).catch(() => {});
        }
        const screen = await current.attachTerminal(ptyId);
        held.current = true;
        // A snapshot supersedes everything queued behind it — replaying pre-gap
        // bytes after a reset would scroll a stale screen above the fresh one.
        preroll.current = [];
        dropped.current = false;
        emit({
          kind: 'reset',
          base64: toBase64(new Uint8Array(screen.replay)),
          cols: screen.cols,
          rows: screen.rows,
        });
        setLoading(false);
      } catch (e) {
        setError(describeError(e));
        setLoading(false);
      }
    })();
    // Published so a teardown for this PTY can queue behind it rather than race
    // it. Keyed by PTY, not by hook instance: the race is between the screen
    // being left and the one replacing it, which are two different instances.
    attachInFlight.set(ptyId, run);
    await run;
    if (attachInFlight.get(ptyId) === run) attachInFlight.delete(ptyId);
  }, [ptyId, emit]);

  useEffect(() => {
    if (!client) return;
    let live = true;
    // Remounting the same screen reuses these refs, so re-arm rather than
    // assuming the initial value still holds.
    alive.current = true;

    client.setTerminalSink({
      onOutput: (id: string, bytes: ArrayBuffer) => {
        // One sink serves every attached terminal, so frames for a screen this
        // hook does not own must be ignored rather than rendered here.
        if (!live || id !== ptyId) return;
        emit({ kind: 'write', base64: toBase64(new Uint8Array(bytes)) });
      },
      onGap: (id: string) => {
        if (!live || id !== ptyId) return;
        // Re-attach for a fresh snapshot. Not surfaced as an error: this is
        // recovery working, not a failure the user needs to act on.
        void attach();
      },
      onExit: (id: string, code: number | undefined) => {
        if (!live || id !== ptyId) return;
        emit({ kind: 'write', base64: toBase64(exitNotice(code)) });
      },
    });

    void attach();

    return () => {
      live = false;
      alive.current = false;
      held.current = false;
      writerRef.current = null;
      preroll.current = [];
      // Detach behind the in-flight attach rather than beside it. Both are async
      // and addressed to the same PTY, so firing this immediately lets a teardown
      // land after the next screen's attach and tear *that* one down instead —
      // leaving a terminal that renders its snapshot and then never updates.
      const pending = attachInFlight.get(ptyId);
      const stop = () => clientRef.current?.detachTerminal(ptyId).catch(() => {});
      if (pending) void pending.then(stop, stop);
      else stop();
    };
  }, [client, ptyId, attach, emit]);

  const subscribe = useCallback(
    (writer: FrameWriter) => {
      writerRef.current = writer;
      // A writer arriving after frames were dropped, or replacing one that
      // already consumed the pre-roll (the page reloaded), is looking at a blank
      // emulator with no way to reconstruct what it missed. Re-attach for a
      // fresh snapshot instead of streaming into a screen with a hole in it.
      if (dropped.current || served.current) {
        dropped.current = false;
        preroll.current = [];
        void attach();
      } else {
        const queued = preroll.current;
        preroll.current = [];
        for (const frame of queued) writer(frame);
      }
      served.current = true;
      return () => {
        if (writerRef.current === writer) writerRef.current = null;
      };
    },
    [attach]
  );

  const send = useCallback(
    (data: string) => {
      if (!alive.current) return;
      // xterm hands back the exact bytes a real terminal would send, already
      // encoded — so each code unit is one byte and must not be re-encoded.
      const bytes = Uint8Array.from(data, (c) => c.charCodeAt(0) & 0xff);
      // Chain the sends. Each one is an independent async RPC, so firing them
      // concurrently lets two keystrokes reach the host out of order — typing
      // "echo " fast enough arrives as "eho c". A terminal's input is a byte
      // *stream*; order is the one thing it cannot be sloppy about.
      sendChain.current = sendChain.current
        .then(() => clientRef.current?.sendTerminalInput(ptyId, toArrayBuffer(bytes)))
        .then(
          () => {},
          (e: unknown) => setError(describeError(e))
        );
    },
    [ptyId]
  );

  const resize = useCallback(
    (cols: number, rows: number) => {
      // Same reasoning as `send`: a stale page still runs its fit logic, and a
      // resize from a screen the user has left would drag the host's terminal
      // to the size of a view nobody is looking at.
      if (!alive.current) return;
      // Failures are swallowed: a read-only device is refused every resize, and
      // an error row that reappears on every rotation would be noise reporting
      // a tier the user already chose.
      clientRef.current?.resizeTerminal(ptyId, cols, rows).catch(() => {});
    },
    [ptyId]
  );

  return {
    subscribe,
    error,
    loading,
    send,
    resize,
    resync: () => void attach(),
    dismissError: () => setError(undefined),
  };
}

/**
 * The dim italic line the emulator shows when the process ends, so a dead shell
 * reads as finished rather than as a terminal that stopped responding.
 */
function exitNotice(code: number | undefined): Uint8Array {
  const suffix = code === undefined ? '' : ` with ${code}`;
  const text = `\r\n\x1b[2m[process exited${suffix}]\x1b[0m\r\n`;
  return Uint8Array.from(text, (c) => c.charCodeAt(0));
}

/** List the host's terminals. */
export function useTerminalList(): {
  terminals: TerminalInfo[];
  loading: boolean;
  error?: string;
  reload: () => void;
} {
  const client = useClient((s) => s.client);
  const [terminals, setTerminals] = useState<TerminalInfo[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string>();
  const [nonce, setNonce] = useState(0);

  useEffect(() => {
    if (!client) return;
    let live = true;
    setLoading(true);
    client
      .listTerminals()
      .then((rows: TerminalInfo[]) => {
        if (!live) return;
        setTerminals(rows);
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

  return { terminals, loading, error, reload: () => setNonce((n) => n + 1) };
}
