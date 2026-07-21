import { PermissionReply, type ThreadSnapshot } from 'oximux-core';
import { useCallback, useEffect, useRef, useState } from 'react';

import { toChatImage, type Attachment } from './attachments';
import { useClient } from './client';
import { describeError } from './errors';
import {
  EMPTY_THREAD,
  parseThread,
  type AskQuestion,
  type PermissionSuggestion,
  type QuestionAnswers,
  type Thread,
} from './thread';

export type SessionView = {
  thread: Thread;
  /** True until the first snapshot lands — the core pushes one even for an
   * empty session, so this is a real signal rather than a guess. */
  loading: boolean;
  /** A failed action (send/resolve/cancel), shown inline and dismissible. */
  error?: string;
  /** Resolves `false` if the send failed, so the composer can keep the text. */
  send: (text: string, images?: Attachment[]) => Promise<boolean>;
  /** Guide a turn that is already running, instead of queueing a new prompt. */
  steer: (text: string) => Promise<boolean>;
  cancel: () => Promise<boolean>;
  allow: (requestId: string, toolInput: unknown) => Promise<boolean>;
  allowWith: (
    requestId: string,
    toolInput: unknown,
    suggestion: PermissionSuggestion
  ) => Promise<boolean>;
  deny: (requestId: string, message: string) => Promise<boolean>;
  answer: (
    requestId: string,
    questions: AskQuestion[],
    answers: QuestionAnswers
  ) => Promise<boolean>;
  /**
   * Rewind to a user turn: drop it and everything after it.
   *
   * `ordinal` counts **user** entries only, matching the wire. Nothing is
   * applied locally — the truncation arrives as a `Rewound` event that folds
   * through the same subscription, so the phone and the desktop agree by
   * construction rather than by two implementations happening to match.
   */
  rewind: (ordinal: number) => Promise<boolean>;
  dismissError: () => void;
  /** Surface a failure that happened outside an RPC (picking an image, say)
   * through the same inline banner, so the screen has one error channel. */
  reportError: (message: string) => void;
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
    (text: string, images: Attachment[] = []) =>
      run((c) => c.sendPrompt(sessionId, text, images.map(toChatImage))),
    [run, sessionId]
  );

  const steer = useCallback(
    (text: string) => run((c) => c.steer(sessionId, text)),
    [run, sessionId]
  );

  const cancel = useCallback(() => run((c) => c.cancel(sessionId)), [run, sessionId]);

  const rewind = useCallback(
    // `includeFiles: false` — the phone never restores the working tree. That
    // discards uncommitted work belonging to whoever is at the desktop, which
    // is not a thing to trigger from a pocket.
    (ordinal: number) => run((c) => c.rewindSession(sessionId, ordinal, false)),
    [run, sessionId]
  );

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

  const allowWith = useCallback(
    (requestId: string, toolInput: unknown, suggestion: PermissionSuggestion) =>
      run((c) =>
        c.resolvePermission(
          sessionId,
          requestId,
          // The suggestion is quoted straight back from the card that displayed
          // it. Its `raw` is opaque here — only the agent that offered it knows
          // what it means — so echoing rather than rebuilding is what keeps the
          // phone from inventing a grant the agent never proposed.
          new PermissionReply.AllowWithSuggestion({
            updatedInputJson: JSON.stringify(toolInput ?? null),
            suggestionJson: JSON.stringify(suggestion),
          })
        )
      ),
    [run, sessionId]
  );

  const deny = useCallback(
    (requestId: string, message: string) =>
      run((c) => c.resolvePermission(sessionId, requestId, new PermissionReply.Deny({ message }))),
    [run, sessionId]
  );

  const answer = useCallback(
    (requestId: string, questions: AskQuestion[], answers: QuestionAnswers) =>
      run((c) =>
        // Both payloads cross as JSON: the questions are quoted straight back
        // from the snapshot that produced this card, so the core sees the exact
        // shapes the fold emitted rather than a re-derived approximation.
        c.answerQuestion(
          sessionId,
          requestId,
          JSON.stringify(questions),
          JSON.stringify(answers)
        )
      ),
    [run, sessionId]
  );

  const dismissError = useCallback(() => setError(undefined), []);
  const reportError = useCallback((message: string) => setError(message), []);

  return {
    thread,
    loading,
    error,
    send,
    steer,
    cancel,
    allow,
    allowWith,
    deny,
    answer,
    rewind,
    dismissError,
    reportError,
  };
}
