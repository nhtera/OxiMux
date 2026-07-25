import { useClient } from './client';

type Evidence = {
  /** A `subscribe` that resolved — the host answered, so it had this session. */
  subscribed: boolean;
  /** The host answered `UnknownSession` to that subscribe. */
  unknownSession: boolean;
};

/**
 * Whether the desktop has closed this session.
 *
 * Two independent signals, because neither alone covers both ways a phone finds
 * out that a tab went away:
 *
 * - **It vanished from the pushed session list.** This is the case where the
 *   screen was already open when the desktop closed the tab: no per-session
 *   frame announces it — the registry drops the handle, so the event stream just
 *   ends — but the list is push-driven and loses the row immediately.
 * - **The host answered `UnknownSession`.** This is the case where the screen
 *   was opened against a row that had already gone, which the list can no longer
 *   explain because the row left it before the screen mounted.
 *
 * Absence from the list only counts once the host has confirmed the session
 * existed. Otherwise a session created from the phone would read as closed for
 * the moment between navigating into it and its first list push landing — the
 * row is legitimately absent then, and calling that "closed" would be wrong in
 * exactly the flow that creates sessions. A resolved subscribe is the
 * confirmation, rather than having seen the row: the host answering about the
 * session is stronger evidence than a list that may not have caught up.
 *
 * And absence is evidence at all only while the link is live — a phone that
 * cannot reach the desktop knows nothing about what the desktop still has open,
 * so every other phase reports the session as present rather than guessing.
 */
export function useSessionGone(sessionId: string, evidence: Evidence): boolean {
  const sessions = useClient((s) => s.sessions);
  const phase = useClient((s) => s.phase);
  const listed = sessions.some((s) => s.sessionId === sessionId);

  return evidence.unknownSession || (evidence.subscribed && phase === 'connected' && !listed);
}
