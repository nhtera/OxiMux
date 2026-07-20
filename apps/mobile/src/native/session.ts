import { PermissionReply, type ThreadSnapshot } from 'oximux-core';
import { useCallback, useEffect, useRef, useState } from 'react';

import { useClient } from './client';
import { describeError } from './errors';
import { EMPTY_THREAD, parseThread, type Thread } from './thread';

export type SessionView = {
  thread: Thread;
  /** True until the first snapshot lands — the core pushes one even for an
   * empty session, so this is a real signal rather than a guess. */
  loading: boolean;
  /** A failed action (send/resolve/cancel), shown inline and dismissible. */
  error?: string;
  /** Resolves `false` if the send failed, so the composer can keep the text. */
  send: (text: string) => Promise<boolean>;
  /** Guide a turn that is already running, instead of queueing a new prompt. */
  steer: (text: string) => Promise<boolean>;
  cancel: () => Promise<boolean>;
  allow: (requestId: string, toolInput: unknown) => Promise<boolean>;
  deny: (requestId: string, message: string) => Promise<boolean>;
  dismissError: () => void;
};

/**
 * Subscribe to one session and expose its folded transcript plus the actions
 * that drive it.
 *
 * The thread arrives already folded from the Rust core, so this hook only
 * parses and stores it — there is no reduction here, deliberately: the fold is
 * `agent-core`'s, shared with the desktop.
 *
 * Note the core has no `unsubscribe`: leaving a session leaves its subscription
 * registered until the client disconnects, and it keeps folding in the
 * background. That is why the sink below guards on `live` — a subscription
 * outliving its screen would otherwise push into an unmounted component.
 * Re-entering a session is *not* free, though: `subscribe` re-fetches the
 * backlog from seq 0 and folds a fresh thread, so the cost scales with the
 * transcript rather than being a cheap re-attach.
 */
export function useSession(sessionId: string): SessionView {
  const client = useClient((s) => s.client);
  const [thread, setThread] = useState<Thread>(EMPTY_THREAD);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string>();
  // Actions read the client through a ref so a reconnect (which swaps the client)
  // does not have to re-create every callback the composer holds.
  const clientRef = useRef(client);
  clientRef.current = client;

  useEffect(() => {
    if (!client) return;
    let live = true;
    // The highest fold cursor rendered so far. Snapshots are whole-state, so an
    // older one arriving late (a re-subscribe racing the live dispatcher after a
    // reconnect) would visibly rewind the transcript. Dropping anything below the
    // high-water mark makes the rendered thread monotonic.
    let renderedSeq = -1;
    setLoading(true);

    const sink = {
      onThread: (snapshot: ThreadSnapshot) => {
        if (!live) return;
        const seq = Number(snapshot.seq);
        if (seq < renderedSeq) return;
        renderedSeq = seq;
        // A snapshot that will not parse must not blank the transcript: keep
        // rendering the last good one rather than dropping to an empty screen.
        try {
          setThread(parseThread(snapshot.threadJson));
          setLoading(false);
        } catch (e) {
          setError(describeError(e));
        }
      },
    };

    client.subscribe(sessionId, sink).catch((e: unknown) => {
      if (!live) return;
      setError(describeError(e));
      setLoading(false);
    });

    return () => {
      live = false;
    };
  }, [client, sessionId]);

  /**
   * Run an action, surfacing failure inline rather than throwing into render.
   * Reports whether it succeeded so a caller holding user input (the composer)
   * can decide whether it is safe to discard it.
   */
  const run = useCallback(
    async (action: (c: NonNullable<typeof client>) => Promise<unknown>): Promise<boolean> => {
      const current = clientRef.current;
      if (!current) {
        setError('Not connected to the desktop.');
        return false;
      }
      try {
        await action(current);
        return true;
      } catch (e) {
        setError(describeError(e));
        return false;
      }
    },
    []
  );

  const send = useCallback(
    (text: string) => run((c) => c.sendPrompt(sessionId, text)),
    [run, sessionId]
  );

  const steer = useCallback(
    (text: string) => run((c) => c.steer(sessionId, text)),
    [run, sessionId]
  );

  const cancel = useCallback(() => run((c) => c.cancel(sessionId)), [run, sessionId]);

  const allow = useCallback(
    (requestId: string, toolInput: unknown) =>
      run((c) =>
        c.resolvePermission(
          sessionId,
          requestId,
          // The allow MUST echo the tool's input back: the CLI treats an allow
          // without it as malformed and denies the tool instead. The input we
          // echo is the one already on the tool call this request belongs to.
          new PermissionReply.Allow({ updatedInputJson: JSON.stringify(toolInput ?? null) })
        )
      ),
    [run, sessionId]
  );

  const deny = useCallback(
    (requestId: string, message: string) =>
      run((c) => c.resolvePermission(sessionId, requestId, new PermissionReply.Deny({ message }))),
    [run, sessionId]
  );

  const dismissError = useCallback(() => setError(undefined), []);

  return { thread, loading, error, send, steer, cancel, allow, deny, dismissError };
}
