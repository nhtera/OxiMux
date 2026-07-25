import type { TerminalInfo } from 'oximux-core';
import { useCallback, useEffect, useRef, useState } from 'react';

import { toArrayBuffer, toBase64 } from './base64';
import { useClient } from './client';
import { describeError } from './errors';

/** What the screen needs to render one attached terminal. */
export type TerminalView = {
  /** Base64 bytes to write, newest last. Consumed and cleared by the renderer. */
  pending: Frame[];
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

/**
 * Attach to one terminal and expose its byte stream.
 *
 * Deliberately not stateful beyond a queue: the emulator lives in the WebView
 * and owns the screen, so keeping a second representation here would be a
 * parallel model to drift out of sync with the one the user is looking at.
 *
 * The gap signal is honoured rather than logged. It travels the whole way from
 * the relay daemon's dropped write, through the host and the FFI, to here — and
 * the only correct response is to re-attach and replace the screen, because a
 * gapped terminal is rendering a hole it cannot detect on its own.
 */
export function useTerminal(ptyId: string): TerminalView {
  const client = useClient((s) => s.client);
  const [pending, setPending] = useState<Frame[]>([]);
  const [error, setError] = useState<string>();
  const [loading, setLoading] = useState(true);
  const clientRef = useRef(client);
  clientRef.current = client;

  const attach = useCallback(async () => {
    const current = clientRef.current;
    if (!current) {
      setError('Not connected to the desktop.');
      setLoading(false);
      return;
    }
    try {
      const screen = await current.attachTerminal(ptyId);
      setPending((q) => [
        ...q,
        {
          kind: 'reset',
          base64: toBase64(new Uint8Array(screen.replay)),
          cols: screen.cols,
          rows: screen.rows,
        },
      ]);
      setLoading(false);
    } catch (e) {
      setError(describeError(e));
      setLoading(false);
    }
  }, [ptyId]);

  useEffect(() => {
    if (!client) return;
    let live = true;

    client.setTerminalSink({
      onOutput: (id: string, bytes: ArrayBuffer) => {
        // One sink serves every attached terminal, so frames for a screen this
        // hook does not own must be ignored rather than rendered here.
        if (!live || id !== ptyId) return;
        setPending((q) => [...q, { kind: 'write', base64: toBase64(new Uint8Array(bytes)) }]);
      },
      onGap: (id: string) => {
        if (!live || id !== ptyId) return;
        // Re-attach for a fresh snapshot. Not surfaced as an error: this is
        // recovery working, not a failure the user needs to act on.
        void attach();
      },
      onExit: (id: string, code: number | undefined) => {
        if (!live || id !== ptyId) return;
        setPending((q) => [
          ...q,
          {
            kind: 'write',
            base64: toBase64(exitNotice(code)),
          },
        ]);
      },
    });

    void attach();

    return () => {
      live = false;
      // Best-effort: tell the host to stop streaming. A failure here is not
      // worth surfacing — the screen is already gone.
      clientRef.current?.detachTerminal(ptyId).catch(() => {});
    };
  }, [client, ptyId, attach]);

  const send = useCallback(
    (data: string) => {
      // xterm hands back the exact bytes a real terminal would send, already
      // encoded — so each code unit is one byte and must not be re-encoded.
      const bytes = Uint8Array.from(data, (c) => c.charCodeAt(0) & 0xff);
      clientRef.current?.sendTerminalInput(ptyId, toArrayBuffer(bytes)).catch((e: unknown) => {
        setError(describeError(e));
      });
    },
    [ptyId]
  );

  const resize = useCallback(
    (cols: number, rows: number) => {
      // Failures are swallowed: a read-only device is refused every resize, and
      // an error row that reappears on every rotation would be noise reporting
      // a tier the user already chose.
      clientRef.current?.resizeTerminal(ptyId, cols, rows).catch(() => {});
    },
    [ptyId]
  );

  return {
    pending,
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
