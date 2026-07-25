import { renderHook } from '@testing-library/react-native';

import { useSessionGone } from '@/native/session-presence';

/**
 * The rule that decides a session has been closed on the desktop.
 *
 * Worth testing rather than trying by hand: the wrong answers are the expensive
 * ones — claiming a live session is closed hides a working screen behind a
 * dead-end, and each mistaken branch needs a different real-world race to
 * reproduce (a network drop, a just-created session, a tab closed while the
 * phone watches).
 *
 * `mock`-prefixed names because a `jest.mock` factory is hoisted above them and
 * may only reference out-of-scope bindings that start with that prefix.
 */
let mockPhase = 'connected';
let mockSessions: { sessionId: string }[] = [];

jest.mock('@/native/client', () => ({
  useClient: (selector: (s: unknown) => unknown) =>
    selector({ phase: mockPhase, sessions: mockSessions }),
}));

beforeEach(() => {
  mockPhase = 'connected';
  mockSessions = [{ sessionId: 'agent-1' }];
});

const gone = (evidence: { subscribed: boolean; unknownSession: boolean }) =>
  renderHook(() => useSessionGone('agent-1', evidence)).result.current;

it('reports a listed session as open', () => {
  expect(gone({ subscribed: true, unknownSession: false })).toBe(false);
});

it('reports a session the host dropped from the list as closed', () => {
  mockSessions = [{ sessionId: 'agent-2' }];

  expect(gone({ subscribed: true, unknownSession: false })).toBe(true);
});

it('reports a session the host does not know as closed', () => {
  mockSessions = [];

  // No subscribe ever succeeded here — the screen opened against a row that had
  // already gone, which is the case the list cannot explain.
  expect(gone({ subscribed: false, unknownSession: true })).toBe(true);
});

it('does not call a session closed before the host has confirmed it exists', () => {
  // A session created from the phone, navigated into before its first list push
  // lands. Absent from the list, but nothing has said it is gone.
  mockSessions = [];

  expect(gone({ subscribed: false, unknownSession: false })).toBe(false);
});

it('does not call a session closed while the link is down', () => {
  // A stale list proves nothing about what the desktop still has open.
  mockSessions = [];
  mockPhase = 'reconnecting';

  expect(gone({ subscribed: true, unknownSession: false })).toBe(false);
});
