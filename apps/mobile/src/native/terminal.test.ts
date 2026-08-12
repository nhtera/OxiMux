import { act, renderHook, waitFor } from '@testing-library/react-native';

import { useClient } from '@/native/client';
import { useTerminal, type Frame } from '@/native/terminal';

/**
 * The output path, which carries every byte the user sees.
 *
 * `mock`-prefixed names because a `jest.mock` factory is hoisted above them and
 * may only reference out-of-scope bindings starting with that prefix. The native
 * core is stubbed because importing it for real pulls in the `OximuxCore`
 * TurboModule, which does not exist under Node.
 */
jest.mock('oximux-core', () => ({ MobileClient: class {} }));

type Sink = {
  onOutput: (id: string, bytes: ArrayBuffer) => void;
  onGap: (id: string) => void;
  onExit: (id: string, code: number | undefined) => void;
};

const PTY = 'pty-1';

function fakeClient() {
  let sink: Sink | undefined;
  const attachTerminal = jest.fn(async () => ({
    replay: new Uint8Array([0x68, 0x69]).buffer, // "hi"
    cols: 120,
    rows: 40,
  }));
  return {
    attachTerminal,
    detachTerminal: jest.fn(async () => {}),
    sendTerminalInput: jest.fn(async () => {}),
    resizeTerminal: jest.fn(async () => {}),
    setTerminalSink: (s: Sink) => {
      sink = s;
    },
    emitOutput: (text: string) =>
      sink?.onOutput(PTY, Uint8Array.from(text, (c) => c.charCodeAt(0)).buffer),
    gap: () => sink?.onGap(PTY),
    /** Output addressed to a different terminal — one sink serves them all. */
    emitOther: () => sink?.onOutput('pty-other', new Uint8Array([0x41]).buffer),
  };
}

function mount(client: ReturnType<typeof fakeClient>) {
  // The hook reads the live client out of the store rather than a prop.
  useClient.setState({ client: client as never });
  return renderHook(() => useTerminal(PTY));
}

afterEach(() => {
  useClient.setState({ client: undefined });
});

describe('useTerminal', () => {
  it('holds frames until the emulator exists, then drains them in order', async () => {
    const client = fakeClient();
    const { result } = mount(client);
    await waitFor(() => expect(result.current.loading).toBe(false));
    act(() => client.emitOutput('after'));

    // Nothing may be written before a writer registers: the WebView is the only
    // thing holding the screen, so a frame delivered to no one is simply lost.
    const written: Frame[] = [];
    act(() => {
      result.current.subscribe((f) => written.push(f));
    });

    expect(written).toHaveLength(2);
    expect(written[0]).toMatchObject({ kind: 'reset', cols: 120, rows: 40 });
    expect(written[1]).toMatchObject({ kind: 'write' });
  });

  it('streams straight to the writer once registered', async () => {
    const client = fakeClient();
    const { result } = mount(client);
    await waitFor(() => expect(result.current.loading).toBe(false));

    const written: Frame[] = [];
    act(() => {
      result.current.subscribe((f) => written.push(f));
    });
    const drained = written.length;
    act(() => client.emitOutput('live'));
    expect(written).toHaveLength(drained + 1);
  });

  it('ignores output addressed to another terminal', async () => {
    const client = fakeClient();
    const { result } = mount(client);
    await waitFor(() => expect(result.current.loading).toBe(false));

    const written: Frame[] = [];
    act(() => {
      result.current.subscribe((f) => written.push(f));
    });
    const drained = written.length;
    act(() => client.emitOther());
    expect(written).toHaveLength(drained);
  });

  it('re-attaches on a gap rather than rendering a hole', async () => {
    const client = fakeClient();
    const { result } = mount(client);
    await waitFor(() => expect(result.current.loading).toBe(false));
    act(() => {
      result.current.subscribe(() => {});
    });

    expect(client.attachTerminal).toHaveBeenCalledTimes(1);
    act(() => client.gap());
    // A gap means bytes were dropped upstream; only a fresh snapshot can say
    // what the screen actually looks like now.
    await waitFor(() => expect(client.attachTerminal).toHaveBeenCalledTimes(2));
  });

  it('re-attaches when a second writer registers', async () => {
    const client = fakeClient();
    const { result } = mount(client);
    await waitFor(() => expect(result.current.loading).toBe(false));

    let release = () => {};
    act(() => {
      release = result.current.subscribe(() => {});
    });
    act(() => release());

    // A replacement writer is a reloaded page staring at a blank emulator. The
    // pre-roll was already consumed, so nothing local can repaint it.
    const written: Frame[] = [];
    act(() => {
      result.current.subscribe((f) => written.push(f));
    });
    await waitFor(() => expect(client.attachTerminal).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(written[0]).toMatchObject({ kind: 'reset' }));
  });

  it('resyncs instead of buffering without limit', async () => {
    const client = fakeClient();
    const { result } = mount(client);
    await waitFor(() => expect(result.current.loading).toBe(false));

    // A terminal producing output faster than the page can boot must not be able
    // to grow this array until the app dies.
    act(() => {
      for (let i = 0; i < 400; i++) client.emitOutput('x');
    });

    const written: Frame[] = [];
    act(() => {
      result.current.subscribe((f) => written.push(f));
    });
    expect(written.length).toBeLessThan(400);
    await waitFor(() => expect(client.attachTerminal).toHaveBeenCalledTimes(2));
  });

  it('releases the old attachment before taking a new one', async () => {
    const client = fakeClient();
    const { result } = mount(client);
    await waitFor(() => expect(result.current.loading).toBe(false));
    act(() => {
      result.current.subscribe(() => {});
    });

    act(() => result.current.resync());
    await waitFor(() => expect(client.attachTerminal).toHaveBeenCalledTimes(2));
    // Attaching is not idempotent — the host fans output out once per
    // attachment, so a re-attach that keeps the previous one makes every byte
    // arrive twice for the rest of the session.
    expect(client.detachTerminal).toHaveBeenCalledTimes(1);
  });

  it('goes silent once the screen is torn down', async () => {
    const client = fakeClient();
    const { result, unmount } = mount(client);
    await waitFor(() => expect(result.current.loading).toBe(false));
    const send = result.current.send;
    const resize = result.current.resize;

    unmount();
    // iOS does not reliably reap a WebView's content process on unmount, so the
    // page — and its emulator's onData handler — can outlive the screen. Every
    // survivor forwards the same keystroke, which is how one keypress reached
    // the shell eight times after eight visits. A torn-down screen must not be
    // able to type into the user's terminal, or resize it.
    act(() => send('rm -rf something'));
    act(() => resize(40, 20));
    expect(client.sendTerminalInput).not.toHaveBeenCalled();
    expect(client.resizeTerminal).not.toHaveBeenCalled();
  });

  it('keeps keystrokes in order when sends are slow', async () => {
    const client = fakeClient();
    // Make the first RPC settle last. Fired concurrently, "ab" would reach the
    // host as "ba" — which is how fast typing arrived as `echo ` → `eho c`.
    const gate: (() => void)[] = [];
    client.sendTerminalInput.mockImplementation(
      () => new Promise<void>((resolve) => gate.push(resolve)) as never
    );
    const { result } = mount(client);
    await waitFor(() => expect(result.current.loading).toBe(false));

    await act(async () => {
      result.current.send('a');
      result.current.send('b');
    });
    // Only the first is in flight; the second must be waiting on it.
    expect(client.sendTerminalInput).toHaveBeenCalledTimes(1);
    await act(async () => {
      gate[0]?.();
    });
    expect(client.sendTerminalInput).toHaveBeenCalledTimes(2);

    const order = client.sendTerminalInput.mock.calls.map(
      (c) => String.fromCharCode(...new Uint8Array((c as unknown as [string, ArrayBuffer])[1]))
    );
    expect(order).toEqual(['a', 'b']);
  });

  it('sends each code unit as one byte', async () => {
    const client = fakeClient();
    const { result } = mount(client);
    await waitFor(() => expect(result.current.loading).toBe(false));

    // xterm hands back bytes, already encoded — re-encoding an escape sequence
    // as UTF-8 would turn a control byte into two.
    await act(async () => result.current.send('\x1b[A'));
    const [, payload] = client.sendTerminalInput.mock.calls[0] as unknown as [string, ArrayBuffer];
    expect(Array.from(new Uint8Array(payload))).toEqual([0x1b, 0x5b, 0x41]);
  });
});
