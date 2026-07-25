import { act, renderHook } from '@testing-library/react-native';

import { useConnectionRecovery } from '@/hooks/use-connection-recovery';

/**
 * The retry loop that keeps a phone on a changing network from stranding itself.
 *
 * Worth testing rather than trying by hand: the failure it exists for takes a
 * real handover between two networks to reproduce, the simulator cannot produce
 * one, and the delay it must widen is only observable over minutes.
 *
 * `mock`-prefixed names because a `jest.mock` factory is hoisted above them and
 * may only reference out-of-scope bindings that start with that prefix.
 */
const mockEnsureConnected = jest.fn();
let mockPhase = 'connected';

jest.mock('@/native/client', () => ({
  useClient: (selector: (s: unknown) => unknown) =>
    selector({ phase: mockPhase, ensureConnected: mockEnsureConnected }),
}));

beforeEach(() => {
  jest.useFakeTimers();
  mockEnsureConnected.mockClear();
  mockPhase = 'connected';
});

afterEach(() => {
  jest.useRealTimers();
});

it('dials once on mount, so a reloaded screen is not left unpaired', () => {
  renderHook(() => useConnectionRecovery());

  expect(mockEnsureConnected).toHaveBeenCalledTimes(1);
});

it('does not keep dialling while the link is live', () => {
  renderHook(() => useConnectionRecovery());
  act(() => jest.advanceTimersByTime(10 * 60_000));

  // Only the mount call. A timer ticking against a healthy connection would be
  // pure battery cost.
  expect(mockEnsureConnected).toHaveBeenCalledTimes(1);
});

it('retries after the core has given up', () => {
  mockPhase = 'unreachable';
  renderHook(() => useConnectionRecovery());
  mockEnsureConnected.mockClear();

  act(() => jest.advanceTimersByTime(4_999));
  expect(mockEnsureConnected).not.toHaveBeenCalled();

  act(() => jest.advanceTimersByTime(1));
  expect(mockEnsureConnected).toHaveBeenCalledTimes(1);
});

it('widens the wait across consecutive failures', () => {
  mockPhase = 'unreachable';
  const { rerender } = renderHook(() => useConnectionRecovery());
  mockEnsureConnected.mockClear();

  act(() => jest.advanceTimersByTime(5_000));
  expect(mockEnsureConnected).toHaveBeenCalledTimes(1);

  // A retry passes through `connecting` before failing back. This round trip is
  // the reason the attempt count lives in a ref: scoped to the effect it would
  // reset here, and every wait would stay 5s forever.
  mockPhase = 'connecting';
  rerender({});
  mockPhase = 'unreachable';
  rerender({});

  act(() => jest.advanceTimersByTime(5_000));
  expect(mockEnsureConnected).toHaveBeenCalledTimes(1);
  act(() => jest.advanceTimersByTime(5_000));
  expect(mockEnsureConnected).toHaveBeenCalledTimes(2);
});

it('stops retrying once the link comes back, and starts fresh if it drops again', () => {
  mockPhase = 'unreachable';
  const { rerender } = renderHook(() => useConnectionRecovery());
  mockEnsureConnected.mockClear();

  act(() => jest.advanceTimersByTime(5_000));
  expect(mockEnsureConnected).toHaveBeenCalledTimes(1);

  mockPhase = 'connected';
  rerender({});
  act(() => jest.advanceTimersByTime(60_000));
  expect(mockEnsureConnected).toHaveBeenCalledTimes(1);

  // A later drop is a new incident, not a continuation of the old backoff: the
  // user who just walked into signal should not wait out a minute-wide delay.
  mockPhase = 'disconnected';
  rerender({});
  act(() => jest.advanceTimersByTime(5_000));
  expect(mockEnsureConnected).toHaveBeenCalledTimes(2);
});
