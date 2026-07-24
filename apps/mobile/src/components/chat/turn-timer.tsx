import { useEffect, useRef, useState } from 'react';
import { StyleSheet } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Spacing } from '@/constants/theme';

/**
 * A coarse elapsed readout — `"0s"`, `"12s"`, `"1m 04s"`. This is a *felt* signal
 * ("the agent is working, and roughly how long"), not a stopwatch, so it rounds to
 * whole seconds and never shows milliseconds.
 */
export function formatDuration(ms: number): string {
  const totalSec = Math.max(0, Math.round(ms / 1000));
  if (totalSec < 60) return `${totalSec}s`;
  const minutes = Math.floor(totalSec / 60);
  const seconds = totalSec % 60;
  return `${minutes}m ${String(seconds).padStart(2, '0')}s`;
}

/**
 * A live turn clock. While a turn runs it ticks `Working {elapsed}`; when the turn
 * finishes it freezes to `Worked for {elapsed}` so the number the user watched
 * climb stays put rather than vanishing.
 *
 * The start instant is captured *locally* the moment this observes the turn go
 * active — the thread projection carries no turn-start timestamp yet, so a turn
 * that was already running when the screen opened (or after a reconnect) can only
 * show an indeterminate `Working…`. Surfacing a real start time across reconnects
 * would need a field on the Rust projection; this covers the common "watch it
 * work" case without one.
 */
export function TurnTimer({ active }: { active: boolean }) {
  const wasActive = useRef(false);
  // Held as state, not a ref, so the render below reads only state — the compiler
  // (rightly) forbids reading a ref or calling `Date.now()` during render, so all
  // clock reads happen inside effects/interval callbacks and land here as state.
  const [startedAt, setStartedAt] = useState<number | null>(null);
  const [elapsedMs, setElapsedMs] = useState(0);
  const [frozenMs, setFrozenMs] = useState<number | null>(null);

  useEffect(() => {
    if (active && !wasActive.current) {
      // Turn just started (observed): stamp the local start, drop any prior freeze.
      setStartedAt(Date.now());
      setElapsedMs(0);
      setFrozenMs(null);
    } else if (!active && wasActive.current && startedAt != null) {
      // Turn just finished (observed): freeze the elapsed so it stops climbing.
      setFrozenMs(Date.now() - startedAt);
    }
    wasActive.current = active;
  }, [active, startedAt]);

  useEffect(() => {
    if (!active || startedAt == null) return;
    const tick = () => setElapsedMs(Date.now() - startedAt);
    tick();
    const id = setInterval(tick, 1000);
    return () => clearInterval(id);
  }, [active, startedAt]);

  let label: string | null = null;
  if (active) {
    label = startedAt != null ? `Working ${formatDuration(elapsedMs)}` : 'Working…';
  } else if (frozenMs != null) {
    label = `Worked for ${formatDuration(frozenMs)}`;
  }
  if (!label) return null;

  return (
    <ThemedText type="small" themeColor="textMuted" style={styles.timer}>
      {label}
    </ThemedText>
  );
}

const styles = StyleSheet.create({
  // Tabular figures so the ticking second doesn't jitter the line width.
  timer: { paddingHorizontal: Spacing.three, paddingTop: Spacing.one, fontVariant: ['tabular-nums'] },
});
