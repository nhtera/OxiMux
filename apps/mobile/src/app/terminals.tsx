import { router } from 'expo-router';
import type { TerminalInfo } from 'oximux-core';
import { ActivityIndicator, FlatList, Pressable, StyleSheet, View } from 'react-native';
import { SafeAreaView } from 'react-native-safe-area-context';

import { ConnectionBanner } from '@/components/connection-banner';
import { ThemedText } from '@/components/themed-text';
import { ThemedView } from '@/components/themed-view';
import { EmptyState } from '@/components/ui/empty-state';
import { ErrorBanner } from '@/components/ui/error-banner';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { useTerminalList } from '@/native/terminal';

/**
 * The host's terminals.
 *
 * A device the desktop narrowed to a single agent session is refused this whole
 * surface rather than shown an empty list — terminals are not owned by a
 * session, so there is nothing a narrowed scope could map onto. The refusal is
 * explained here rather than surfaced as a bare error, because "you may not"
 * and "there are none" look identical otherwise.
 */
export default function TerminalsScreen() {
  const { terminals, loading, error, reload } = useTerminalList();
  const theme = useTheme();

  return (
    <ThemedView style={styles.fill}>
      <SafeAreaView style={styles.fill} edges={['bottom']}>
        <ConnectionBanner />
        {error ? (
          <View style={styles.notice}>
            {/* Tap retries the list rather than merely clearing the message —
                the affordance the ad-hoc version had — so `onDismiss` is wired
                to `reload`, not a plain dismiss. */}
            <ErrorBanner message={error} onDismiss={reload} />
            <ThemedText type="small" style={{ color: theme.textSecondary }}>
              {error.includes('not authorized') || error.includes('Unauthorized')
                ? 'This device is limited to one agent session, so terminals are not shared with it. Change its access on the desktop, in Settings → Remote.'
                : 'Tap to try again.'}
            </ThemedText>
          </View>
        ) : null}

        {loading ? (
          <View style={styles.centre}>
            <ActivityIndicator />
          </View>
        ) : (
          <FlatList
            data={terminals}
            keyExtractor={(t) => t.ptyId}
            renderItem={({ item }) => <TerminalRow terminal={item} />}
            contentContainerStyle={styles.list}
            ListEmptyComponent={
              error ? null : <EmptyState title="No terminals open on the desktop." />
            }
          />
        )}
      </SafeAreaView>
    </ThemedView>
  );
}

function TerminalRow({ terminal }: { terminal: TerminalInfo }) {
  const theme = useTheme();
  return (
    <Pressable
      onPress={() => router.push({ pathname: '/terminal/[id]', params: { id: terminal.ptyId } })}
      style={styles.row}
    >
      <ThemedText type="smallBold">{shortCwd(terminal.cwd)}</ThemedText>
      <ThemedText type="small" style={{ color: theme.textSecondary }}>
        {terminal.cols}×{terminal.rows}
      </ThemedText>
    </Pressable>
  );
}

/**
 * The tail of a path. A terminal's identity to its user is the directory it is
 * in, and on a phone the leading `/Users/name/Code/…` is the part that never
 * varies — so it is the part worth dropping.
 */
function shortCwd(cwd: string): string {
  const parts = cwd.split('/').filter(Boolean);
  return parts.slice(-2).join('/') || cwd;
}

const styles = StyleSheet.create({
  fill: { flex: 1 },
  list: { padding: Spacing.three, gap: Spacing.three },
  row: { gap: Spacing.one },
  centre: { flex: 1, alignItems: 'center', justifyContent: 'center', padding: Spacing.four },
  notice: { paddingHorizontal: Spacing.three, paddingVertical: Spacing.two, gap: Spacing.one },
});
