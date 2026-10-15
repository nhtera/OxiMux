import { useCallback, useEffect, useMemo, useRef } from 'react';
import { StyleSheet, View } from 'react-native';
import WebView, { type WebViewMessageEvent } from 'react-native-webview';

import type { ViewWriter } from '@/native/simulator';

import { SIM_VIEW_BASE, simPage } from './sim-page';

type Props = {
  /** Register the page's writer once it is up; returns an unsubscribe. */
  subscribe: (writer: ViewWriter) => () => void;
  /** The first picture is on screen. */
  onFirstFrame?: () => void;
  /** This WebView cannot decode H.264 (no WebCodecs). */
  onUnsupported?: () => void;
  /** The page cannot go on without a fresh config and key frame: its
   *  decoder failed, or fell behind. */
  onStuck?: () => void;
};

/** Whether the WebView may load `url`: the page itself, nothing else. */
export function mayLoad(url: string): boolean {
  return url === SIM_VIEW_BASE || url.startsWith('about:');
}

/**
 * A watched device's screen: the decode page in a WebView, fed frame by
 * frame. Watching only: the page takes no touches to pass on.
 */
export function SimView({ subscribe, onFirstFrame, onUnsupported, onStuck }: Props) {
  const webRef = useRef<WebView>(null);
  const unsubscribe = useRef<(() => void) | null>(null);
  const seq = useRef(0);
  // Built once: a new `source` reloads the page and drops the picture.
  const source = useMemo(() => ({ html: simPage('#000000'), baseUrl: SIM_VIEW_BASE }), []);

  useEffect(
    () => () => {
      unsubscribe.current?.();
      unsubscribe.current = null;
    },
    []
  );

  const onMessage = useCallback(
    (event: WebViewMessageEvent) => {
      let msg: { type: string; webcodecs?: boolean };
      try {
        msg = JSON.parse(event.nativeEvent.data);
      } catch {
        return;
      }
      if (msg.type === 'ready') {
        if (msg.webcodecs === false) {
          onUnsupported?.();
          return;
        }
        // Registered on ready, not on mount, so the first config reaches a
        // decoder that exists. A reloaded page registers again and gets the
        // config, then waits for the next key frame.
        unsubscribe.current?.();
        unsubscribe.current = subscribe((frame) => {
          webRef.current?.postMessage(JSON.stringify({ ...frame, seq: seq.current++ }));
        });
      } else if (msg.type === 'first') {
        onFirstFrame?.();
      } else if (msg.type === 'unsupported') {
        onUnsupported?.();
      } else if (msg.type === 'error' || msg.type === 'stuck') {
        onStuck?.();
      }
    },
    [subscribe, onFirstFrame, onUnsupported, onStuck]
  );

  return (
    <View style={styles.fill}>
      <WebView
        ref={webRef}
        source={source}
        onMessage={onMessage}
        originWhitelist={[SIM_VIEW_BASE.replace(/\/$/, ''), 'about:*']}
        onShouldStartLoadWithRequest={(req) => mayLoad(req.url)}
        javaScriptEnabled
        scrollEnabled={false}
        bounces={false}
        style={styles.web}
        containerStyle={styles.fill}
      />
    </View>
  );
}

const styles = StyleSheet.create({
  fill: { flex: 1, backgroundColor: '#000' },
  web: { flex: 1, backgroundColor: '#000' },
});
