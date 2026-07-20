import { router } from 'expo-router';
import type { TerminalInfo } from 'oximux-core';
import { ActivityIndicator, FlatList, Pressable, StyleSheet, View } from 'react-native';
import { SafeAreaView } from 'react-native-safe-area-context';

import { ThemedText } from '@/components/themed-text';
import { ThemedView } from '@/components/themed-view';
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
        {error ? (
          <Pressable onPress={reload} style={styles.notice}>
            <ThemedText type="small" style={styles.errorText}>
              {error}
            </ThemedText>
            <ThemedText type="small" style={{ color: theme.textSecondary }}>
              {error.includes('not authorized') || error.includes('Unauthorized')
                ? 'This device is limited to one agent session, so terminals are not shared with it. Change its access on the desktop, in Settings → Remote.'
                : 'Tap to try again.'}
            </ThemedText>
          </Pressable>
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
              error ? null : (
                <View style={styles.centre}>
                  <ThemedText style={{ color: theme.textSecondary }}>
                    No terminals open on the desktop.
                  </ThemedText>
                </View>
              )
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
  errorText: { color: '#F85149' },
});
