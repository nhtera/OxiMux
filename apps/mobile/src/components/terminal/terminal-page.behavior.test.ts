/**
 * @jest-environment jsdom
 */
import { MIN_FONT, terminalPage, type FitMode } from './terminal-page';

/**
 * The bridge script, actually run.
 *
 * The sibling suite proves it parses; this one proves it behaves — above all
 * that mirror mode never posts a resize. That is not a cosmetic preference: the
 * relay runs each PTY at the smallest size any attachment asks for, so a resize
 * posted from the phone narrows the desktop's terminal to phone width for as
 * long as the phone is attached. A string match cannot tell you the branch is
 * unreachable; running it can.
 */

type Posted = { type: string; [key: string]: unknown };

/** Menlo's advance width is ~0.6em; the page measures rather than assumes it,
 *  so the fake has to model the relationship for the fit maths to mean anything. */
const ADVANCE = 0.6;
const LINE_HEIGHT = 1.2;

class FakeTerminal {
  options: { fontSize: number };
  cols = 80;
  rows = 24;
  resets = 0;
  focused = 0;
  writes: Uint8Array[] = [];
  scrolled: number[] = [];
  _core: { _renderService: { dimensions: unknown } };
  private onDataCb: ((data: string) => void) | undefined;

  constructor(options: { fontSize: number }) {
    this.options = { ...options };
    const self = this;
    this._core = {
      _renderService: {
        get dimensions() {
          return {
            css: {
              cell: {
                width: self.options.fontSize * ADVANCE,
                height: self.options.fontSize * LINE_HEIGHT,
              },
            },
          };
        },
      },
    };
  }

  open() {}
  onData(cb: (data: string) => void) {
    this.onDataCb = cb;
  }
  /** Drive a keystroke the way xterm would after the soft keyboard types. */
  press(data: string) {
    this.onDataCb?.(data);
  }
  write(bytes: Uint8Array) {
    this.writes.push(bytes);
  }
  reset() {
    this.resets++;
  }
  resize(cols: number, rows: number) {
    this.cols = cols;
    this.rows = rows;
  }
  focus() {
    this.focused++;
  }
  scrollLines(n: number) {
    this.scrolled.push(n);
  }
}

const VIEWPORT = { width: 393, height: 768 };

function boot(mode: FitMode) {
  document.body.innerHTML = '<div id="term"></div>';
  const posted: Posted[] = [];
  let term!: FakeTerminal;

  Object.defineProperty(window, 'innerWidth', { value: VIEWPORT.width, configurable: true });
  Object.defineProperty(window, 'innerHeight', { value: VIEWPORT.height, configurable: true });
  (globalThis as Record<string, unknown>).Terminal = function (opts: { fontSize: number }) {
    term = new FakeTerminal(opts);
    return term;
  };
  (globalThis as Record<string, unknown>).ReactNativeWebView = {
    postMessage: (raw: string) => posted.push(JSON.parse(raw)),
  };
  (window as unknown as Record<string, unknown>).ReactNativeWebView = (
    globalThis as Record<string, unknown>
  ).ReactNativeWebView;
  // Run frame callbacks inline so a refit settles within the test tick. The page
  // nests two on purpose — changing the font changes the metrics the next
  // measurement needs — and both must run for the size to converge.
  (globalThis as Record<string, unknown>).requestAnimationFrame = (cb: () => void) => {
    cb();
    return 0;
  };

  // jsdom keeps one window for the whole file, and the page registers listeners
  // on it that outlive the test that booted them — and `post` resolves
  // `ReactNativeWebView` at call time, so a stale page happily answers a later
  // test's messages into its array. Record what each boot registers so it can be
  // torn down again.
  const registered: [EventTarget, string, EventListener][] = [];
  const targets: EventTarget[] = [window, document];
  const originals = targets.map((t) => t.addEventListener);
  targets.forEach((target) => {
    const original = target.addEventListener.bind(target);
    target.addEventListener = (type: string, listener: EventListener) => {
      registered.push([target, type, listener]);
      original(type, listener);
    };
  });

  const html = terminalPage('#000000', '#ffffff', mode);
  const open = html.lastIndexOf('<script>');
  const close = html.lastIndexOf('</script>');
  try {
    new Function(html.slice(open + '<script>'.length, close))();
  } finally {
    targets.forEach((target, i) => {
      target.addEventListener = originals[i];
    });
  }

  // Mirrors TerminalView: every message carries a monotonic seq, and it is
  // delivered to BOTH targets, which is what a real device does.
  let seq = 0;
  const send = (msg: object) => {
    const raw = JSON.stringify({ ...msg, seq: seq++ });
    window.dispatchEvent(new MessageEvent('message', { data: raw }));
    document.dispatchEvent(Object.assign(new Event('message'), { data: raw }));
  };
  const dispose = () => {
    for (const [target, type, listener] of registered) target.removeEventListener(type, listener);
  };
  live.push(dispose);
  return {
    posted,
    send,
    dispose,
    get term() {
      return term;
    },
    typesOf: (type: string) => posted.filter((p) => p.type === type),
  };
}

/** Teardowns for pages booted this test. */
const live: (() => void)[] = [];

afterEach(() => {
  while (live.length) live.pop()?.();
  delete (globalThis as Record<string, unknown>).Terminal;
  delete (globalThis as Record<string, unknown>).ReactNativeWebView;
});

describe('the terminal page, running', () => {
  it('never asks the host to resize while mirroring', () => {
    const page = boot('mirror');
    page.send({ type: 'reset', cols: 120, rows: 40 });
    page.send({ type: 'write', data: btoa('hello') });

    // The whole point: attaching from a phone must leave the desktop's terminal
    // exactly as wide as it was.
    expect(page.typesOf('resize')).toHaveLength(0);
  });

  it('keeps the host grid and shrinks the font to fit instead', () => {
    const page = boot('mirror');
    page.send({ type: 'reset', cols: 120, rows: 40 });

    // The emulator stays at the host's dimensions, so absolute-position bytes
    // land in the cells the host addressed.
    expect(page.term.cols).toBe(120);
    expect(page.term.rows).toBe(40);
    // 120 columns cannot fit 393pt at a readable size, so the font bottoms out
    // at the floor and the rest becomes something to pan over.
    expect(page.term.options.fontSize).toBe(MIN_FONT);
    const geometry = page.typesOf('geometry').at(-1);
    expect(geometry).toMatchObject({ cols: 120, rows: 40, overflow: true });
  });

  it('grows the font when the host grid is narrow enough to fit', () => {
    const page = boot('mirror');
    page.send({ type: 'reset', cols: 40, rows: 20 });

    expect(page.term.options.fontSize).toBeGreaterThan(MIN_FONT);
    expect(page.typesOf('geometry').at(-1)).toMatchObject({ overflow: false });
  });

  it('does resize the host in reflow mode', () => {
    const page = boot('reflow');
    page.send({ type: 'reset', cols: 120, rows: 40 });

    const resizes = page.typesOf('resize');
    expect(resizes.length).toBeGreaterThan(0);
    // Fitted to the phone: 393pt at a 12pt font's advance width.
    expect(resizes.at(-1)).toMatchObject({ cols: Math.floor(VIEWPORT.width / (12 * ADVANCE)) });
    expect(page.term.cols).toBeLessThan(120);
  });

  it('starts resizing only once switched to reflow', () => {
    const page = boot('mirror');
    page.send({ type: 'reset', cols: 120, rows: 40 });
    expect(page.typesOf('resize')).toHaveLength(0);

    page.send({ type: 'mode', mode: 'reflow' });
    expect(page.typesOf('resize').length).toBeGreaterThan(0);
  });

  it('hands the host size back when leaving reflow', () => {
    const page = boot('mirror');
    page.send({ type: 'reset', cols: 120, rows: 40 });
    page.send({ type: 'mode', mode: 'reflow' });
    expect(page.term.cols).toBeLessThan(120);

    page.send({ type: 'mode', mode: 'mirror' });
    // Going quiet is not enough: the daemon keeps each attachment's LAST
    // requested size, so without re-asserting, the phone goes on voting for its
    // narrow grid and the desktop stays shrunk even after the phone detaches.
    expect(page.typesOf('resize').at(-1)).toMatchObject({ cols: 120, rows: 40 });
    expect(page.term.cols).toBe(120);
    expect(page.term.rows).toBe(40);
  });

  it('ignores a mode message that changes nothing', () => {
    const page = boot('mirror');
    page.send({ type: 'reset', cols: 120, rows: 40 });
    // `ready` re-asserts the mode on every page load, so a no-op must stay a
    // no-op — otherwise it would post a restoring resize on every reload.
    page.send({ type: 'mode', mode: 'mirror' });
    expect(page.typesOf('resize')).toHaveLength(0);
  });

  it('applies sticky Ctrl to the next keystroke and then clears it', () => {
    const page = boot('mirror');
    page.send({ type: 'ctrl', on: true });
    page.term.press('c');

    // Ctrl cannot be held on a touch screen, and the keystroke it modifies is
    // typed into xterm's own textarea where React Native never sees it.
    expect(page.typesOf('input').at(-1)).toMatchObject({ data: '\x03' });
    expect(page.typesOf('ctrl').at(-1)).toMatchObject({ on: false });

    // Armed once, not sticky forever.
    page.term.press('c');
    expect(page.typesOf('input').at(-1)).toMatchObject({ data: 'c' });
  });

  it('passes a keystroke with no control codepoint through unchanged', () => {
    const page = boot('mirror');
    page.send({ type: 'ctrl', on: true });
    page.term.press('1');
    expect(page.typesOf('input').at(-1)).toMatchObject({ data: '1' });
  });

  it('clears the screen before replaying a snapshot', () => {
    const page = boot('mirror');
    page.send({ type: 'reset', cols: 80, rows: 24 });
    // Writing a replay on top of the old contents would scroll the pre-gap
    // screen above it, where it reads as real history.
    expect(page.term.resets).toBe(1);
  });

  it('applies a message once even when both listeners fire', () => {
    const page = boot('mirror');
    page.send({ type: 'reset', cols: 80, rows: 24 });
    page.send({ type: 'write', data: btoa('hi') });

    // `boot` delivers to document AND window, as a real device can. Handling a
    // write twice puts every byte the host sent on screen twice, which reads as
    // doubled typing and is what actually shipped before the seq guard.
    expect(page.term.writes).toHaveLength(1);
    expect(page.term.resets).toBe(1);
  });

  it('drives scrollback and focus from the key bar', () => {
    const page = boot('mirror');
    page.send({ type: 'scroll', lines: -10 });
    page.send({ type: 'focus' });
    expect(page.term.scrolled).toEqual([-10]);
    expect(page.term.focused).toBeGreaterThan(0);
  });
});
