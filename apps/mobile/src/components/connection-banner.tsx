import { StyleSheet, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { useClient, type ConnectionPhase } from '@/native/client';

// Only the states worth interrupting for. `connected` and `idle` show nothing —
// an always-on "connected" bar is noise, and idle means the user has not paired.
const MESSAGE: Partial<Record<ConnectionPhase, string>> = {
  connecting: 'Connecting…',
  reconnecting: 'Reconnecting…',
  disconnected: 'Disconnected — changes will not reach the desktop.',
  unreachable: 'Host unreachable',
};

/**
 * A thin ambient bar shown on any primary screen while the link is not live, so a
 * drop is visible where the user is — not only after their next action fails. The
 * full `ConnectionBadge` still lives on Sessions/Settings; this is the terse
 * signal for someone deep in a chat, diff, or schedule.
 */
export function ConnectionBanner() {
  const theme = useTheme();
  const phase = useClient((s) => s.phase);
  const message = MESSAGE[phase];
  if (!message) return null;
  const tone = phase === 'disconnected' || phase === 'unreachable' ? theme.danger : theme.warning;
  return (
    <View style={[styles.bar, { backgroundColor: tone }]}>
      <ThemedText type="small" numberOfLines={1} style={{ color: theme.accentText }}>
        {message}
      </ThemedText>
    </View>
  );
}

const styles = StyleSheet.create({
  bar: { paddingVertical: Spacing.half, paddingHorizontal: Spacing.three, alignItems: 'center' },
});
