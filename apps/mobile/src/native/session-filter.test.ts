import type { SessionSummary } from 'oximux-core';

import { filterSessions } from '@/native/session-filter';

// Built without an `as SessionSummary` cast on purpose: the first version of
// this fixture used one, which silently hid both that `model` is optional rather
// than nullable and that `lastSeq` exists at all. A cast in a fixture buys
// nothing and switches off the check that the fixture matches reality.
const session = (over: Partial<SessionSummary>): SessionSummary => ({
  sessionId: 'id',
  title: 'Untitled',
  lastSeq: 0n,
  awaitingPermission: false,
  ...over,
});

const SESSIONS = [
  session({ sessionId: '1', title: 'Fix the parser', model: 'opus-5' }),
  session({ sessionId: '2', title: 'Write docs', model: 'sonnet-5' }),
  // No model at all — the field is optional on the wire.
  session({ sessionId: '3', title: 'Refactor auth' }),
];

describe('filterSessions', () => {
  it('returns everything for an empty or whitespace query', () => {
    expect(filterSessions(SESSIONS, '')).toHaveLength(3);
    expect(filterSessions(SESSIONS, '   ')).toHaveLength(3);
  });

  it('matches on title, case-insensitively', () => {
    expect(filterSessions(SESSIONS, 'PARSER').map((s) => s.sessionId)).toEqual(['1']);
  });

  it('matches on model too', () => {
    // Sessions are named for what they are doing, not what runs them, so
    // "which of these is on sonnet" is unanswerable from titles alone.
    expect(filterSessions(SESSIONS, 'sonnet').map((s) => s.sessionId)).toEqual(['2']);
  });

  it('tolerates a session with no model', () => {
    expect(() => filterSessions(SESSIONS, 'refactor')).not.toThrow();
    expect(filterSessions(SESSIONS, 'refactor').map((s) => s.sessionId)).toEqual(['3']);
  });

  it('returns nothing when no session matches', () => {
    expect(filterSessions(SESSIONS, 'nonexistent')).toEqual([]);
  });
});
