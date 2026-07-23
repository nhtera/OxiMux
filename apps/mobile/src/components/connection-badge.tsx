import { StyleSheet, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { useClient, type ConnectionPhase } from '@/native/client';

/**
 * Green for a live link, amber while it is being (re)established, red once the
 * core has stopped trying. `reconnecting` reads amber rather than red on purpose:
 * the driver is still working the backoff and the session is not lost yet.
 */
function dotColor(phase: ConnectionPhase, theme: ReturnType<typeof useTheme>): string {
  switch (phase) {
    case 'idle':
      return theme.textMuted;
    case 'connecting':
    case 'reconnecting':
      return theme.warning;
    case 'connected':
      return theme.success;
    case 'disconnected':
    case 'unreachable':
      return theme.danger;
  }
}

const LABEL: Record<ConnectionPhase, string> = {
  idle: 'Not paired',
  connecting: 'Connecting…',
  connected: 'Connected',
  reconnecting: 'Reconnecting…',
  disconnected: 'Disconnected',
  unreachable: 'Host unreachable',
};

export function ConnectionBadge() {
  const theme = useTheme();
  const phase = useClient((s) => s.phase);
  const cause = useClient((s) => s.cause);
  return (
    <View style={styles.row}>
      <View style={[styles.dot, { backgroundColor: dotColor(phase, theme) }]} />
      <ThemedText type="small">
        {LABEL[phase]}
        {cause ? ` — ${cause}` : ''}
      </ThemedText>
    </View>
  );
}

const styles = StyleSheet.create({
  row: { flexDirection: 'row', alignItems: 'center', gap: Spacing.two },
  dot: { width: 8, height: 8, borderRadius: 4 },
});
