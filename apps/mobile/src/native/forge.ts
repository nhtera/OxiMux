/**
 * Presentation logic for the forge surface — issue/PR rows, CI check status,
 * and the prompt text an item is attached with.
 *
 * Unlike the transcript and git models, these arrive as **uniffi records**
 * (camelCase, generated), so there are no serde shapes to mirror here. What
 * lives here instead is the interpretation the host deliberately does not do:
 * bucketing check states, formatting timestamps for the reader's locale, and
 * composing attach text.
 *
 * Pure functions, so the parts that are easy to get quietly wrong — a state
 * string that no longer matches, a relative time that reads "in 3 hours" — are
 * testable without rendering.
 */

import type { CheckRun, ForgeItem } from 'oximux-core';

/** How a check row should read, collapsed from the forge's own bucket string. */
export type CheckStatus = 'pass' | 'fail' | 'pending' | 'skipped' | 'unknown';

/**
 * Interpret a check's bucket.
 *
 * The bucket comes from the forge CLI, which already normalises wildly varying
 * per-provider status strings — so this maps rather than re-derives. An
 * unrecognised value becomes `unknown` rather than being forced into `pending`
 * or `fail`: showing a neutral state is honest, whereas guessing "failed" for
 * something merely unfamiliar would be alarming and wrong.
 */
export function checkStatus(check: CheckRun): CheckStatus {
  switch (check.bucket.toLowerCase()) {
    case 'pass':
    case 'success':
      return 'pass';
    case 'fail':
    case 'failure':
      return 'fail';
    case 'pending':
      return 'pending';
    case 'skipping':
    case 'skipped':
    case 'cancel':
    case 'cancelled':
      return 'skipped';
    default:
      return 'unknown';
  }
}

/** Whether every check has finished — nothing left pending. */
export function checksSettled(checks: CheckRun[]): boolean {
  return checks.every((c) => checkStatus(c) !== 'pending');
}

/**
 * A one-line summary of a check run, e.g. `3 passed, 1 failed`.
 *
 * Empty string for no checks: the caller renders its own empty state, and "0
 * passed" would imply checks ran and none succeeded.
 */
export function checksSummary(checks: CheckRun[]): string {
  if (checks.length === 0) return '';
  const counts = { pass: 0, fail: 0, pending: 0, skipped: 0, unknown: 0 };
  checks.forEach((c) => {
    counts[checkStatus(c)] += 1;
  });
  const parts: string[] = [];
  if (counts.pass) parts.push(`${counts.pass} passed`);
  if (counts.fail) parts.push(`${counts.fail} failed`);
  if (counts.pending) parts.push(`${counts.pending} pending`);
  if (counts.skipped) parts.push(`${counts.skipped} skipped`);
  if (counts.unknown) parts.push(`${counts.unknown} unknown`);
  return parts.join(', ');
}

/**
 * A compact relative age, e.g. `3h`, `2d`. Empty when the forge omitted the
 * timestamp or it will not parse.
 *
 * `now` is injectable so this is testable without freezing the clock globally.
 *
 * A **future** timestamp reads as `now` rather than a negative age. Clock skew
 * between the desktop, the forge and the phone makes this reachable without
 * anything being wrong, and "in 3 hours" on a list of past events reads as a
 * bug.
 */
export function relativeAge(rfc3339: string, now: number = Date.now()): string {
  if (!rfc3339) return '';
  const then = Date.parse(rfc3339);
  if (Number.isNaN(then)) return '';
  const seconds = Math.floor((now - then) / 1000);
  if (seconds < 60) return 'now';
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h`;
  const days = Math.floor(hours / 24);
  if (days < 365) return `${days}d`;
  return `${Math.floor(days / 365)}y`;
}

/**
 * The prompt text an "attach" action inserts.
 *
 * Deliberately plain: a heading line naming the item, the link, then the body
 * verbatim. The agent reads this as ordinary text through the existing
 * `sendPrompt` path — nothing about attaching needs a new RPC or a structured
 * payload, and inventing one would mean the agent had to understand a format
 * this app made up.
 *
 * The body is included when known. `null` (the forge could not supply it) and
 * an empty body are both rendered as just the heading and link, because in
 * neither case is there anything more to say.
 */
export function attachPromptText(item: ForgeItem, body: string | null): string {
  const heading = `${item.title} (#${item.number})`;
  const lines = [heading, item.url];
  const trimmed = (body ?? '').trim();
  if (trimmed) {
    lines.push('', trimmed);
  }
  return lines.join('\n');
}
