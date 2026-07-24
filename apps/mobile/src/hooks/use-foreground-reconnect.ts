import { useEffect } from 'react';
import { AppState } from 'react-native';

import { useClient } from '@/native/client';

/**
 * Re-establish the host link when the app returns to the foreground.
 *
 * The connection is otherwise stood up only once, on the boot screen. iOS
 * suspends a backgrounded app, so its QUIC link goes stale and the reconnect
 * driver spends its budget and gives up — after which nothing redials. Without
 * this, a resumed app sits on "Waiting for the host…" until it is force-quit.
 *
 * Mounted once at the app root, so it also covers a JS reload that restores a
 * screen with the store reset (the boot screen never re-runs): the mount-time
 * call reconnects even without a background→foreground transition.
 *
 * `ensureConnected` is idempotent — it no-ops while the link is live or a dial is
 * already in flight — so firing on both mount and every `active` transition is
 * safe and never double-dials.
 */
export function useForegroundReconnect() {
  const ensureConnected = useClient((s) => s.ensureConnected);
  useEffect(() => {
    void ensureConnected();
    const sub = AppState.addEventListener('change', (state) => {
      if (state === 'active') void ensureConnected();
    });
    return () => sub.remove();
  }, [ensureConnected]);
}
