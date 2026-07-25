import { render, screen } from '@testing-library/react-native';

import { ConnectionBanner } from '@/components/connection-banner';

/**
 * `mock`-prefixed so the hoisted `jest.mock` factory may reference it.
 */
let mockState: { phase: string; cause?: string } = { phase: 'connected' };

jest.mock('@/native/client', () => ({
  useClient: (selector: (s: unknown) => unknown) => selector(mockState),
}));

it('says nothing while the link is live', () => {
  mockState = { phase: 'connected' };
  render(<ConnectionBanner />);

  expect(screen.toJSON()).toBeNull();
});

it('carries the failure reason, not just the fact of failure', () => {
  mockState = { phase: 'unreachable', cause: 'no addressing information' };
  render(<ConnectionBanner />);

  // The whole point of surfacing it: a desktop that is off and a network that
  // cannot route are the same red bar without this text.
  expect(screen.getByText(/Host unreachable — no addressing information/)).toBeTruthy();
});

it('states the phase alone when the core gave no reason', () => {
  mockState = { phase: 'disconnected' };
  render(<ConnectionBanner />);

  expect(screen.getByText(/changes will not reach the desktop/)).toBeTruthy();
});
