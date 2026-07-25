import type { SessionSummary } from 'oximux-core';

/**
 * Sessions matching `query`, case-insensitively, over title and model.
 *
 * Model is searched as well as title because sessions are often named by what
 * they are doing, not by what is running them — "which of these is on Opus" is a
 * question the title alone cannot answer.
 *
 * Client-side by design: `ListSessions` returns everything the host is willing
 * to expose in one response, so the whole list is already here and filtering it
 * needs no round trip.
 */
export function filterSessions(sessions: SessionSummary[], query: string): SessionSummary[] {
  const needle = query.trim().toLowerCase();
  if (needle.length === 0) return sessions;
  return sessions.filter((s) => {
    const title = s.title?.toLowerCase() ?? '';
    const model = s.model?.toLowerCase() ?? '';
    return title.includes(needle) || model.includes(needle);
  });
}
