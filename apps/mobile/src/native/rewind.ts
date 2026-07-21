/**
 * What a rewind would do, computed from the transcript the phone already holds.
 *
 * No RPC enumerates checkpoints, because none is needed: `ThreadEntry.User`
 * already carries its `checkpoint`, so everything the confirm card shows is
 * derivable from the snapshot on screen. Adding a query would put a second,
 * lagging copy of the same facts on the wire.
 *
 * These are pure functions over entries so the arithmetic — which decides how
 * many messages a destructive action removes — is testable without rendering
 * anything.
 */

import { isUser, type ThreadEntry } from '@/native/thread';

/** A turn the user can rewind to. */
export type RewindTarget = {
  /** Position among **user** entries — what the wire takes. */
  ordinal: number;
  /** Position in `entries`, for reading the row back. */
  entryIndex: number;
  /** The message text, so the card can name the turn being returned to. */
  text: string;
  /**
   * How many entries would be dropped: this turn and everything after it.
   *
   * Counts transcript **entries**, not user turns — a turn that ran six tools
   * removes seven rows, and telling the user "1 message" would understate a
   * destructive action.
   */
  messagesRemoved: number;
  /**
   * Whether a files snapshot exists for this turn.
   *
   * Mirrors the desktop's own offer condition: a checkpoint that exists but is
   * not flagged `show` means the turn did not change the repository, so
   * restoring files would be a no-op dressed up as a choice.
   */
  filesAvailable: boolean;
};

/**
 * Every user turn that can be rewound to, in transcript order.
 *
 * The **last** user turn is included. Rewinding to it drops that prompt and its
 * reply, which is a meaningful thing to want (it is what "undo my last message"
 * means) even though nothing follows it.
 */
export function rewindTargets(entries: ThreadEntry[]): RewindTarget[] {
  const targets: RewindTarget[] = [];
  let ordinal = 0;
  entries.forEach((entry, entryIndex) => {
    if (!isUser(entry)) return;
    const { text, checkpoint } = entry.User;
    targets.push({
      ordinal,
      entryIndex,
      text,
      messagesRemoved: entries.length - entryIndex,
      filesAvailable: checkpoint?.show === true,
    });
    ordinal += 1;
  });
  return targets;
}

/** A one-line label for a turn, clipped so a long prompt cannot break the row. */
export function targetLabel(target: RewindTarget, maxChars = 60): string {
  // Newlines collapse first: a multi-line prompt would otherwise render as one
  // very tall row in what is meant to be a scannable list.
  const flat = target.text.replace(/\s+/g, ' ').trim();
  if (flat.length === 0) return '(empty message)';
  return flat.length <= maxChars ? flat : `${flat.slice(0, maxChars - 1)}…`;
}
