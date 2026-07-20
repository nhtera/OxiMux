import { useCallback, useEffect, useRef } from 'react';
import { StyleSheet, View } from 'react-native';
import { WebView, type WebViewMessageEvent } from 'react-native-webview';

import { useTheme } from '@/hooks/use-theme';
import type { Frame } from '@/native/terminal';

import { terminalPage } from './terminal-page';

type Props = {
  frames: Frame[];
  onInput: (data: string) => void;
  onResize: (cols: number, rows: number) => void;
};

/**
 * The terminal surface: an xterm.js emulator in a WebView, fed raw bytes.
 *
 * The frame queue is drained by index rather than cleared by the parent. The
 * WebView is the only thing holding the rendered screen, so a re-render that
 * replayed already-written frames would duplicate output — and one that cleared
 * the queue before the page was ready would lose the replay snapshot entirely.
 * A high-water mark survives both.
 */
export function TerminalView({ frames, onInput, onResize }: Props) {
  const theme = useTheme();
  const webRef = useRef<WebView>(null);
  const written = useRef(0);
  const ready = useRef(false);

  const flush = useCallback(() => {
    if (!ready.current || !webRef.current) return;
    if (frames.length > written.current) console.log('[probe] flush', written.current, '->', frames.length);
    for (let i = written.current; i < frames.length; i++) {
      const frame = frames[i];
      const message =
        frame.kind === 'reset'
          ? { type: 'reset', cols: frame.cols, rows: frame.rows }
          : null;
      // A reset clears the screen and sizes the grid, then the snapshot bytes
      // are written into it — two messages, because the page must not try to
      // parse a payload out of a control instruction.
      if (message) webRef.current.postMessage(JSON.stringify(message));
      webRef.current.postMessage(JSON.stringify({ type: 'write', data: frame.base64 }));
    }
    written.current = frames.length;
  }, [frames]);

  useEffect(flush, [flush]);

  const onMessage = useCallback(
    (event: WebViewMessageEvent) => {
      let msg: { type: string; data?: string; cols?: number; rows?: number };
      try {
        msg = JSON.parse(event.nativeEvent.data);
      } catch {
        return;
      }
      if (msg.type === 'ready') {
        ready.current = true;
        flush();
      } else if (msg.type === 'input' && msg.data !== undefined) {
        onInput(msg.data);
      } else if (msg.type === 'resize' && msg.cols && msg.rows) {
        onResize(msg.cols, msg.rows);
      }
    },
    [flush, onInput, onResize]
  );

  return (
    <View style={[styles.fill, { backgroundColor: theme.background }]}>
      <WebView
        ref={webRef}
        source={{ html: terminalPage(theme.background, theme.text) }}
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
