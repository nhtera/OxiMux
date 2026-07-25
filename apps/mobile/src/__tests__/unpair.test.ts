import { useClient } from '@/native/client';

/**
 * Forgetting a desktop has to reach the desktop, or its paired-devices list goes
 * on listing a phone that has already left — the row the user then has to clear
 * by hand, wondering whether the button worked.
 *
 * `mock`-prefixed names because a `jest.mock` factory is hoisted above them and
 * may only reference out-of-scope bindings starting with that prefix. The native
 * core is stubbed because importing it for real pulls in the `OximuxCore`
 * TurboModule, which does not exist under Node.
 */
const mockCalls: string[] = [];
const mockClearHost = jest.fn(() => {
  mockCalls.push('clearHost');
  return Promise.resolve();
});

jest.mock('oximux-core', () => ({
  ConnState_Tags: {
    Connecting: 'Connecting',
    Connected: 'Connected',
    Reconnecting: 'Reconnecting',
    Disconnected: 'Disconnected',
    Unreachable: 'Unreachable',
  },
  MobileClient: class {},
}));
jest.mock('@/native/hosts', () => ({
  // Called through rather than passed directly: the `import` above is hoisted
  // over these declarations, so the factory would capture an undefined binding.
  clearHost: () => mockClearHost(),
  loadHost: jest.fn(),
  saveHost: jest.fn(),
  endpointIdBytes: jest.fn(),
}));
jest.mock('@/native/identity', () => ({ getOrCreateSeed: jest.fn() }));

/** A connected client that records the order its methods are called in. */
function fakeClient(unpair: () => Promise<void>) {
  return {
    unpair: jest.fn(() => {
      mockCalls.push('unpair');
      return unpair();
    }),
    disconnect: jest.fn(() => {
      mockCalls.push('disconnect');
      return Promise.resolve();
    }),
  };
}

beforeEach(() => {
  mockCalls.length = 0;
  mockClearHost.mockClear();
});

test('tells the desktop before dropping the connection it would need to tell it over', async () => {
  const client = fakeClient(() => Promise.resolve());
  useClient.setState({ client: client as never, phase: 'connected' });

  await useClient.getState().unpair();

  // Order is the whole point: the RPC has to go out while the link is still up.
  expect(mockCalls).toEqual(['unpair', 'disconnect', 'clearHost']);
  expect(useClient.getState().phase).toBe('idle');
});

test('forgets the desktop locally even when the desktop cannot be told', async () => {
  const client = fakeClient(() => Promise.reject(new Error('transport: unreachable')));
  useClient.setState({ client: client as never, phase: 'unreachable' });

  await expect(useClient.getState().unpair()).resolves.toBeUndefined();

  // A desktop that is asleep, unreachable, or too old to know the call must not
  // be able to keep this phone enrolled. The cost is a stale row on that desktop,
  // which its own Forget clears; the alternative is a phone that cannot leave.
  expect(mockCalls).toEqual(['unpair', 'disconnect', 'clearHost']);
  expect(useClient.getState().client).toBeUndefined();
});
