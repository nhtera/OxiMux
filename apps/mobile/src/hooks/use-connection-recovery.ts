import { useEffect, useRef } from 'react';
import { AppState } from 'react-native';

import { useClient } from '@/native/client';

/**
 * The first wait after the core gives up, and the ceiling the wait doubles to:
 * 5s, 10s, 20s, 40s, then 60s forever. Short enough that a handover back onto a
 * working network recovers while the user is still looking at the screen; capped
 * so a desktop that is off for the afternoon is not dialled every five seconds.
 */
const RETRY_BASE_MS = 5_000;
const RETRY_MAX_MS = 60_000;

/** The phases that mean "no link, and nothing in the core is still trying". */
function isDown(phase: string): boolean {
  return phase === 'disconnected' || phase === 'unreachable';
}

/**
 * Keep the host link alive across the two ways the phone loses it: being
 * suspended, and changing networks.
 *
 * **Foreground.** iOS suspends a backgrounded app, so its QUIC link goes stale
 * while the reconnect driver spends its budget and gives up — after which nothing
 * redials. Without the `AppState` listener a resumed app sits on "Waiting for the
 * host…" until it is force-quit. The mount-time call covers a JS reload too,
 * which restores a screen with the store reset and never re-runs the boot screen.
 *
 * **Network change.** The core's driver retries three times over ~7s and then
 * latches `Unreachable`. That budget is spent long before a Wi-Fi → cellular
 * handover finishes, so walking out of the house with the app open used to leave
 * it dead on screen with the app never having been backgrounded — no foreground
 * transition would ever fire, and only a manual Retry brought it back. The
 * retry effect below keeps dialling on a widening delay for exactly that case.
 *
 * Deliberately not driven by a connectivity listener: that would mean a native
 * module (and the rebuild it implies) to learn something a cheap timer discovers
 * a few seconds later anyway. If the delay is ever felt, that is the upgrade.
 *
 * Both paths go through `ensureConnected`, which no-ops while the link is live or
 * a dial is in flight, so overlapping triggers never double-dial.
 */
export function useConnectionRecovery() {
  const ensureConnected = useClient((s) => s.ensureConnected);
  const phase = useClient((s) => s.phase);

  useEffect(() => {
    void ensureConnected();
    const sub = AppState.addEventListener('change', (state) => {
      if (state === 'active') void ensureConnected();
    });
    return () => sub.remove();
  }, [ensureConnected]);

  /**
   * Consecutive failed dials. A ref, not state: it must survive the render caused
   * by the very phase change it counts. Each retry passes through `connecting`
   * on its way back to a down phase, so an attempt counter scoped to the effect
   * below would reset on every attempt and the delay would never widen.
   */
  const attempts = useRef(0);
  useEffect(() => {
    if (phase === 'connected') attempts.current = 0;
  }, [phase]);

  const down = isDown(phase);
  useEffect(() => {
    if (!down) return;
    const id = setTimeout(
      () => {
        attempts.current += 1;
        void ensureConnected();
      },
      Math.min(RETRY_BASE_MS * 2 ** attempts.current, RETRY_MAX_MS)
    );
    // Cancelled when the dial starts (leaving the down phase) and re-armed at the
    // wider delay if it fails, so exactly one retry is ever in flight.
    return () => clearTimeout(id);
  }, [down, ensureConnected]);
}
