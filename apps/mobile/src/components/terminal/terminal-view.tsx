import {
  useCallback,
  useEffect,
  useImperativeHandle,
  useMemo,
  useRef,
  useState,
  type RefObject,
} from 'react';
import { StyleSheet, View } from 'react-native';
import { WebView, type WebViewMessageEvent } from 'react-native-webview';

import { useTheme } from '@/hooks/use-theme';
import type { FrameWriter } from '@/native/terminal';

import { terminalPage, type FitMode } from './terminal-page';

/** What the phone is showing, so the screen can label it and warn about clipping. */
export type Geometry = { cols: number; rows: number; fontSize: number; overflow: boolean };

/** The controls the key bar drives, reaching into the emulator. */
export type TerminalHandle = {
  /** Raise the soft keyboard and put the cursor back after a key-bar tap. */
  focus: () => void;
  /** Step the font size, in points. */
  zoom: (delta: number) => void;
  /** Arm or clear sticky Ctrl. Applied in the page — the soft keyboard types
   *  into xterm's own textarea, which nothing on this side sees. */
  setCtrl: (on: boolean) => void;
  /** Scroll the scrollback by whole lines (negative is up). */
  scroll: (lines: number) => void;
};

type Props = {
  subscribe: (writer: FrameWriter) => () => void;
  onInput: (data: string) => void;
  onResize: (cols: number, rows: number) => void;
  onGeometry: (geometry: Geometry) => void;
  /** Cleared by the page when a sticky Ctrl is consumed by the next keystroke. */
  onCtrlChange: (on: boolean) => void;
  mode: FitMode;
  ref?: RefObject<TerminalHandle | null>;
};

/**
 * The terminal surface: an xterm.js emulator in a WebView, fed raw bytes.
 *
 * Frames are pushed in through a writer registered on `ready` rather than passed
 * as a prop. The WebView is the only thing holding the rendered screen, so
 * frames must reach it exactly once — and a prop would mean re-rendering this
 * component once per output chunk to deliver them, which a terminal under load
 * does hundreds of times a second.
 */
export function TerminalView({
  subscribe,
  onInput,
  onResize,
  onGeometry,
  onCtrlChange,
  mode,
  ref,
}: Props) {
  const theme = useTheme();
  const webRef = useRef<WebView>(null);
  const unsubscribe = useRef<(() => void) | null>(null);
  const modeRef = useRef(mode);

  // Monotonic per view. The page registers a 'message' listener on both
  // `document` and `window` because the platforms disagree about which one
  // react-native-webview delivers to; on a platform that fires both, every
  // message would otherwise be applied twice — and applying a `write` twice
  // means the user sees every byte the host sends duplicated.
  const seq = useRef(0);
  const send = useCallback((msg: object) => {
    webRef.current?.postMessage(JSON.stringify({ ...msg, seq: seq.current++ }));
  }, []);

  // Built once, and deliberately NOT from `mode`.
  //
  // `source` is a fresh object on every render, and react-native-webview reloads
  // the page whenever it changes — which drops the rendered screen and, for as
  // long as both page instances are alive, gives the emulator two `onData`
  // handlers, so every keystroke reaches the host twice. Memoising on the theme
  // strings keeps one page for the life of the view; mode changes are pushed as
  // a message instead.
  // Lazy initial state, not a ref: the mode the page is BUILT with is captured
  // once and never updated, and reading a ref during render is what this pins
  // down without tripping the rules-of-hooks lint.
  const [initialMode] = useState(mode);
  const source = useMemo(
    () => ({ html: terminalPage(theme.background, theme.text, initialMode) }),
    [theme.background, theme.text, initialMode]
  );

  useImperativeHandle(
    ref,
    () => ({
      focus: () => send({ type: 'focus' }),
      zoom: (delta: number) => send({ type: 'zoom', delta }),
      setCtrl: (on: boolean) => send({ type: 'ctrl', on }),
      scroll: (lines: number) => send({ type: 'scroll', lines }),
    }),
    [send]
  );

  // The page reads its mode at build time, so a later change has to be pushed.
  // Re-rendering the WebView with new HTML instead would drop the screen.
  useEffect(() => {
    // Kept here so `ready` can re-assert the current mode after a page reload
    // without reading state that a stale closure would have captured.
    modeRef.current = mode;
    send({ type: 'mode', mode });
  }, [mode, send]);

  useEffect(
    () => () => {
      unsubscribe.current?.();
      unsubscribe.current = null;
    },
    []
  );

  const onMessage = useCallback(
    (event: WebViewMessageEvent) => {
      let msg: {
        type: string;
        data?: string;
        cols?: number;
        rows?: number;
        fontSize?: number;
        overflow?: boolean;
        on?: boolean;
      };
      try {
        msg = JSON.parse(event.nativeEvent.data);
      } catch {
        return;
      }
      if (msg.type === 'ready') {
        // A page that reloaded for any reason comes back at its build-time mode,
        // which is only the initial one. Re-assert on every ready so the toggle
        // survives rather than silently reverting.
        send({ type: 'mode', mode: modeRef.current });
        // Registering here rather than on mount is what guarantees the replay
        // snapshot is written into an emulator that exists.
        unsubscribe.current?.();
        unsubscribe.current = subscribe((frame) => {
          if (frame.kind === 'reset') {
            // Two messages, because the page must not try to parse a payload out
            // of a control instruction: the reset clears the screen and sizes the
            // grid, then the snapshot bytes are written into it.
            send({ type: 'reset', cols: frame.cols, rows: frame.rows });
          }
          send({ type: 'write', data: frame.base64 });
        });
      } else if (msg.type === 'input' && msg.data !== undefined) {
        onInput(msg.data);
      } else if (msg.type === 'resize' && msg.cols && msg.rows) {
        onResize(msg.cols, msg.rows);
      } else if (msg.type === 'geometry' && msg.cols && msg.rows) {
        onGeometry({
          cols: msg.cols,
          rows: msg.rows,
          fontSize: msg.fontSize ?? 0,
          overflow: !!msg.overflow,
        });
      } else if (msg.type === 'ctrl') {
        onCtrlChange(!!msg.on);
      }
    },
    [subscribe, send, onInput, onResize, onGeometry, onCtrlChange]
  );

  return (
    <View style={[styles.fill, { backgroundColor: theme.background }]}>
      <WebView
        ref={webRef}
        source={source}
        onMessage={onMessage}
        // The page is a fixed local string with no navigation of its own; a
        // terminal must never follow a link that output happened to contain.
        // The inline document itself loads as `about:blank`, so a blanket
        // `false` here blocks the page from ever appearing — allow that one and
        // refuse everything else.
        originWhitelist={['about:']}
        onShouldStartLoadWithRequest={(req) => req.url.startsWith('about:')}
        javaScriptEnabled
        // Without this the soft keyboard shoves the whole page up and the grid
        // is measured against a viewport that no longer matches what is drawn.
        automaticallyAdjustContentInsets={false}
        keyboardDisplayRequiresUserAction={false}
        hideKeyboardAccessoryView
        style={styles.web}
        containerStyle={styles.fill}
      />
    </View>
  );
}

const styles = StyleSheet.create({
  fill: { flex: 1 },
  web: { flex: 1, backgroundColor: 'transparent' },
});
