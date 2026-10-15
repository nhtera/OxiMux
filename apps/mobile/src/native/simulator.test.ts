import { act, renderHook, waitFor } from '@testing-library/react-native';

import { useClient } from '@/native/client';
import {
  FRAME_CONFIG,
  FRAME_DELTA,
  FRAME_KEY,
  PENDING_RETRY_MS,
  ViewBuffer,
  phaseOf,
  useSimView,
  type ViewFrame,
} from '@/native/simulator';

/**
 * Watching a desktop device: what a subscribe's answer means, which frames
 * reach the page, and that a view off screen costs the desktop nothing.
 *
 * The core is stubbed (its TurboModule does not exist under Node); the tags
 * mirror the generated ones.
 */
jest.mock('oximux-core', () => ({
  MobileClient: class {},
  SimSubscribeOutcome_Tags: { Started: 'Started', Pending: 'Pending', Denied: 'Denied', Refused: 'Refused' },
}));

type Sink = {
  onFrame: (token: string, kind: number, pts: bigint, data: ArrayBuffer) => void;
  onEnd: (token: string) => void;
};

const started = (token: string) => ({ tag: 'Started', inner: { token, width: 590, height: 1278 } });
const pending = { tag: 'Pending' };

function fakeClient(answers: object[]) {
  let sink: Sink | undefined;
  const subscribeSimView = jest.fn(async () => answers.shift() ?? started('t-late'));
  return {
    subscribeSimView,
    unsubscribeSimView: jest.fn(async () => {}),
    connectionRelayed: jest.fn(() => true),
    setSimViewSink: (s: Sink) => {
      sink = s;
    },
    frame: (token: string, kind: number) => sink?.onFrame(token, kind, 0n, new Uint8Array([kind, 9]).buffer),
    end: (token: string) => sink?.onEnd(token),
  };
}

function mount(client: ReturnType<typeof fakeClient>, active = true) {
  useClient.setState({ client: client as never });
  return renderHook(({ on }: { on: boolean }) => useSimView('E0E0', on), { initialProps: { on: active } });
}

afterEach(() => {
  useClient.setState({ client: undefined });
  jest.useRealTimers();
});

describe('phaseOf', () => {
  it('maps every desktop answer to what the screen shows', () => {
    expect(phaseOf(started('t1') as never)).toEqual({ phase: { kind: 'live', width: 590, height: 1278 }, token: 't1' });
    expect(phaseOf(pending as never).phase.kind).toBe('waiting');
    const denied = phaseOf({ tag: 'Denied', inner: { retryAfterSecs: 42n } } as never).phase;
    expect(denied).toMatchObject({ kind: 'denied', retryMs: 42_000 });
    expect(phaseOf({ tag: 'Refused', inner: { message: 'Viewing is off.' } } as never).phase).toEqual({
      kind: 'refused',
      message: 'Viewing is off.',
    });
  });
});

describe('ViewBuffer', () => {
  const f = (k: number, b = String(k)): ViewFrame => ({ k, b });

  it('hands over only what decodes: the config, then the last key frame and what follows it', () => {
    const buffer = new ViewBuffer();
    buffer.push(f(FRAME_DELTA, 'orphan'));
    buffer.push(f(FRAME_CONFIG));
    buffer.push(f(FRAME_DELTA, 'before key'));
    buffer.push(f(FRAME_KEY, 'k1'));
    buffer.push(f(FRAME_DELTA, 'd1'));
    buffer.push(f(FRAME_KEY, 'k2'));
    buffer.push(f(FRAME_DELTA, 'd2'));
    expect(buffer.drain().map((x) => x.b)).toEqual([String(FRAME_CONFIG), 'k2', 'd2']);
  });

  it('keeps the config for a page that reloads, and takes no delta after it overflowed', () => {
    const buffer = new ViewBuffer(3);
    buffer.push(f(FRAME_CONFIG));
    buffer.push(f(FRAME_KEY, 'k'));
    buffer.push(f(FRAME_DELTA, 'd1'));
    buffer.push(f(FRAME_DELTA, 'd2'));
    buffer.push(f(FRAME_DELTA, 'd3'));
    buffer.push(f(FRAME_DELTA, 'd4'));
    expect(buffer.drain().map((x) => x.b)).toEqual([String(FRAME_CONFIG)]);
    buffer.push(f(FRAME_KEY, 'k2'));
    buffer.push(f(FRAME_DELTA, 'd5'));
    expect(buffer.drain().map((x) => x.b)).toEqual([String(FRAME_CONFIG), 'k2', 'd5']);
    // A reloaded page still gets the config.
    expect(buffer.drain().map((x) => x.b)).toEqual([String(FRAME_CONFIG)]);
  });
});

describe('useSimView', () => {
  it('asks again while the desktop decides, then streams its own frames only', async () => {
    jest.useFakeTimers();
    const client = fakeClient([pending, started('t1')]);
    const { result } = mount(client);
    await waitFor(() => expect(result.current.phase.kind).toBe('waiting'));
    await act(async () => {
      jest.advanceTimersByTime(PENDING_RETRY_MS);
    });
    await waitFor(() => expect(result.current.phase.kind).toBe('live'));
    expect(client.subscribeSimView).toHaveBeenCalledTimes(2);

    act(() => client.frame('t1', FRAME_CONFIG));
    act(() => client.frame('t1', FRAME_KEY));
    const written: ViewFrame[] = [];
    act(() => {
      result.current.subscribe((x) => written.push(x));
    });
    expect(written.map((x) => x.k)).toEqual([FRAME_CONFIG, FRAME_KEY]);
    act(() => client.frame('someone-else', FRAME_DELTA));
    act(() => client.frame('t1', FRAME_DELTA));
    expect(written.map((x) => x.k)).toEqual([FRAME_CONFIG, FRAME_KEY, FRAME_DELTA]);
  });

  it('unsubscribes the moment it goes inactive, and subscribes again on return', async () => {
    const client = fakeClient([started('t1'), started('t2')]);
    const { result, rerender } = mount(client);
    await waitFor(() => expect(result.current.phase.kind).toBe('live'));
    rerender({ on: false });
    expect(client.unsubscribeSimView).toHaveBeenCalledWith('t1');
    expect(result.current.phase.kind).toBe('paused');
    rerender({ on: true });
    await waitFor(() => expect(client.subscribeSimView).toHaveBeenCalledTimes(2));
  });

  it('subscribes again after its view ends (a reconnect, the device restarting)', async () => {
    jest.useFakeTimers();
    const client = fakeClient([started('t1'), started('t2')]);
    const { result } = mount(client);
    await waitFor(() => expect(result.current.phase.kind).toBe('live'));
    act(() => client.end('t1'));
    expect(result.current.phase.kind).toBe('starting');
    await act(async () => {
      jest.advanceTimersByTime(2000);
    });
    await waitFor(() => expect(client.subscribeSimView).toHaveBeenCalledTimes(2));
    expect(result.current.relayed).toBe(true);
  });

  it('keeps the frames that beat the answer naming their token', async () => {
    let answer!: (v: object) => void;
    const client = fakeClient([]);
    client.subscribeSimView.mockImplementationOnce(() => new Promise((r) => (answer = r)));
    const { result } = mount(client);
    await waitFor(() => expect(client.subscribeSimView).toHaveBeenCalledTimes(1));
    // The stream is quicker than the reply.
    act(() => client.frame('t1', FRAME_CONFIG));
    act(() => client.frame('t1', FRAME_KEY));
    act(() => client.frame('t0', FRAME_KEY));
    await act(async () => answer(started('t1')));
    expect(result.current.phase.kind).toBe('live');
    const written: ViewFrame[] = [];
    act(() => {
      result.current.subscribe((x) => written.push(x));
    });
    expect(written.map((x) => x.k)).toEqual([FRAME_CONFIG, FRAME_KEY]);
  });

  it('notices a stream that ended before its answer came, and subscribes again', async () => {
    jest.useFakeTimers();
    let answer!: (v: object) => void;
    const client = fakeClient([started('t2')]);
    client.subscribeSimView.mockImplementationOnce(() => new Promise((r) => (answer = r)));
    const { result } = mount(client);
    await waitFor(() => expect(client.subscribeSimView).toHaveBeenCalledTimes(1));
    act(() => client.end('t1'));
    await act(async () => answer(started('t1')));
    expect(result.current.phase.kind).toBe('starting');
    await act(async () => {
      jest.advanceTimersByTime(2000);
    });
    await waitFor(() => expect(client.subscribeSimView).toHaveBeenCalledTimes(2));
  });

  it('never asks again by itself after the desktop said no', async () => {
    jest.useFakeTimers();
    const client = fakeClient([{ tag: 'Denied', inner: { retryAfterSecs: 1n } }, started('t1')]);
    const { result } = mount(client);
    await waitFor(() => expect(result.current.phase.kind).toBe('denied'));
    await act(async () => {
      jest.advanceTimersByTime(120_000);
    });
    expect(client.subscribeSimView).toHaveBeenCalledTimes(1);
    act(() => result.current.retry());
    await waitFor(() => expect(result.current.phase.kind).toBe('live'));
  });

  it('starts over when the page is stuck, but not more often than every few seconds', async () => {
    const client = fakeClient([started('t1'), started('t2'), started('t3')]);
    const { result } = mount(client);
    await waitFor(() => expect(result.current.phase.kind).toBe('live'));
    act(() => result.current.restart());
    await waitFor(() => expect(client.subscribeSimView).toHaveBeenCalledTimes(2));
    expect(client.unsubscribeSimView).toHaveBeenCalledWith('t1');
    act(() => result.current.restart());
    await waitFor(() => expect(result.current.phase.kind).toBe('live'));
    expect(client.subscribeSimView).toHaveBeenCalledTimes(2);
  });

  it('never subscribes while inactive', async () => {
    const client = fakeClient([started('t1')]);
    const { result } = mount(client, false);
    await waitFor(() => expect(result.current.phase.kind).toBe('paused'));
    expect(client.subscribeSimView).not.toHaveBeenCalled();
  });
});
