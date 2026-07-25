import { act, fireEvent, render, screen, waitFor } from '@testing-library/react-native';

import PairScanScreen from '@/app/pair-scan';
import type { PairStep } from '@/native/client';

const TICKET = 'oximux://connect?ticket=abc';

/*
 * Here rather than beside the screen it covers: everything under `src/app` is a
 * route, so a test file there gets bundled into the app — which drags
 * `@testing-library/react-native` in with it and fails the Metro build outright.
 */

/**
 * `mock`-prefixed names because a `jest.mock` factory is hoisted above them and
 * may only reference out-of-scope bindings starting with that prefix.
 *
 * The client module is stubbed rather than driven, the way the other suites here
 * do it: importing it for real pulls in the `OximuxCore` TurboModule, which does
 * not exist under Node. What is under test is the screen's progress machine, and
 * that only needs `pair` to be a promise it can hold open.
 */
let mockSettle: { resolve: () => void; reject: (e: unknown) => void };
let mockStep: ((s: PairStep) => void) | undefined;
const mockPair = jest.fn((_url: string, onStep?: (s: PairStep) => void) => {
  mockStep = onStep;
  return new Promise<void>((resolve, reject) => {
    mockSettle = { resolve, reject };
  });
});

jest.mock('@/native/client', () => ({
  useClient: (selector: (s: unknown) => unknown) => selector({ pair: mockPair }),
}));

// Off-device the screen opens in manual entry, so most of these drive the paste
// form rather than faking camera frames — it enters the same progress machine a
// scan does. A getter (not a literal) so a test can claim to be a real device
// and exercise the scan path's own failure screen.
let mockIsDevice = false;
jest.mock('expo-device', () => ({
  get isDevice() {
    return mockIsDevice;
  },
}));
// Captures the barcode callback so a test can deliver a frame, which is the only
// way to reach the scan path's own states — manual entry recovers through its
// form instead.
let mockScan: ((result: { data: string }) => void) | undefined;
jest.mock('expo-camera', () => ({
  CameraView: (props: { onBarcodeScanned?: (r: { data: string }) => void }) => {
    mockScan = props.onBarcodeScanned;
    return null;
  },
  useCameraPermissions: () => [{ granted: true }, jest.fn()],
}));
jest.mock('expo-router', () => ({ router: { replace: jest.fn() } }));

function submitTicket() {
  fireEvent.changeText(screen.getByPlaceholderText('oximux://connect?ticket=…'), TICKET);
  fireEvent.press(screen.getByText('Pair'));
}

beforeEach(() => {
  jest.useFakeTimers();
  mockPair.mockClear();
  mockStep = undefined;
  mockIsDevice = false;
});

afterEach(() => {
  jest.useRealTimers();
});

it('reports that it is working instead of leaving the screen unchanged', async () => {
  render(<PairScanScreen />);
  submitTicket();

  // The regression this pins: a scan used to change nothing on screen at all
  // while the dial ran, which is exactly why it read as stuck.
  await waitFor(() => expect(screen.getByText(/Connecting to your Mac/)).toBeTruthy());
});

it('explains the doubled wait when the device was already paired', async () => {
  render(<PairScanScreen />);
  submitTicket();
  await waitFor(() => expect(mockStep).toBeDefined());
  act(() => mockStep?.('resuming'));

  // Two full dials back to back. Unlabelled, the second is indistinguishable
  // from a hang — and an already-paired phone takes this path every time.
  await waitFor(() => expect(screen.getByText(/reconnecting instead/)).toBeTruthy());
});

it('admits a slow dial is expected rather than broken', async () => {
  render(<PairScanScreen />);
  submitTicket();
  await waitFor(() => expect(screen.getByText(/Connecting to your Mac/)).toBeTruthy());

  expect(screen.queryByText(/relay/)).toBeNull();
  act(() => jest.advanceTimersByTime(3_000));

  await waitFor(() => expect(screen.getByText(/relay/)).toBeTruthy());
});

it('gives up rather than waiting forever', async () => {
  render(<PairScanScreen />);
  submitTicket();
  await waitFor(() => expect(screen.getByText(/Connecting to your Mac/)).toBeTruthy());

  // Nothing ever settles this promise. Before the timeout the screen had no
  // failure state to reach: it waited indefinitely with the scanner latched.
  act(() => jest.advanceTimersByTime(30_000));

  await waitFor(() => expect(screen.getByText(/in time/)).toBeTruthy());
});

it('offers a way on when a scan fails, rather than a dead viewfinder', async () => {
  // The scan path specifically: manual entry recovers through its own form (the
  // link is still typed, so "Pair" is the retry), but a failed scan used to drop
  // the user back to a live camera with a banner in the dimmed foot.
  mockIsDevice = true;
  render(<PairScanScreen />);
  await waitFor(() => expect(mockScan).toBeDefined());

  act(() => mockScan?.({ data: TICKET }));
  await waitFor(() => expect(screen.getByText(/Connecting to your Mac/)).toBeTruthy());
  act(() => jest.advanceTimersByTime(30_000));

  await waitFor(() => expect(screen.getByText('Try again')).toBeTruthy());
  expect(screen.getByText(/in time/)).toBeTruthy();
});

it('ignores a QR that is not a pairing link, so a stray code cannot start one', async () => {
  mockIsDevice = true;
  render(<PairScanScreen />);
  await waitFor(() => expect(mockScan).toBeDefined());

  act(() => mockScan?.({ data: 'https://example.com/poster' }));

  expect(mockPair).not.toHaveBeenCalled();
  expect(screen.queryByText(/Connecting to your Mac/)).toBeNull();
});

it('turns a refusal into something the user can act on', async () => {
  render(<PairScanScreen />);
  submitTicket();
  await waitFor(() => expect(screen.getByText(/Connecting to your Mac/)).toBeTruthy());

  await act(async () => {
    mockSettle.reject({ tag: 'Transport', inner: ['unauthorized'] });
  });

  // Not the bare "Transport: unauthorized" the tag alone would render as.
  await waitFor(() => expect(screen.getByText(/refused this code/)).toBeTruthy());
  expect(screen.queryByText(/Transport:/)).toBeNull();
});
