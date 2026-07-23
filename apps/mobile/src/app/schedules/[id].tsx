import { Stack, router, useLocalSearchParams } from 'expo-router';
import { RunOutcome, type ScheduleRun } from 'oximux-core';
import { ActivityIndicator, FlatList, Pressable, RefreshControl, StyleSheet, View } from 'react-native';
import { SafeAreaView } from 'react-native-safe-area-context';

import { ThemedText } from '@/components/themed-text';
import { ThemedView } from '@/components/themed-view';
import { Spacing } from '@/constants/theme';
import { useScheduleRuns } from '@/native/use-schedules';

/**
 * A schedule's run history: when it fired, whether the run started cleanly, and
 * where it went.
 *
 * **Empty is normal.** A schedule that has not reached its first fire, or was
 * just created, simply has no runs — the screen says so rather than treating it
 * as a failure.
 *
 * A run's session opens on the desktop and its id is process-local there, so the
 * phone shows the id as a label but does not link into it: that id does not
 * survive a desktop restart and would route nowhere the next day.
 */
export default function ScheduleRunsScreen() {
  const { id, name } = useLocalSearchParams<{ id: string; name?: string }>();
  const scheduleId = String(id);
  const { runs, loading, error, refresh } = useScheduleRuns(scheduleId);

  return (
    <ThemedView style={styles.fill}>
      <Stack.Screen options={{ title: name ? String(name) : 'Run history' }} />
      <SafeAreaView style={styles.fill} edges={['bottom']}>
        {error ? (
          <ThemedText type="small" style={styles.error} numberOfLines={3}>
            {error}
          </ThemedText>
        ) : null}

        {loading && runs.length === 0 ? (
          <View style={styles.centre}>
            <ActivityIndicator />
          </View>
        ) : (
          <FlatList
            data={runs}
            keyExtractor={(run) => run.firedAt}
            contentContainerStyle={styles.list}
            renderItem={({ item }) => <RunRow run={item} />}
            refreshControl={<RefreshControl refreshing={loading} onRefresh={refresh} />}
            ListEmptyComponent={
              <View style={styles.centre}>
                <ThemedText type="small" style={styles.empty}>
                  No runs yet. This schedule has not fired — it will appear here
                  once it does, provided the desktop app is running at the time.
                </ThemedText>
              </View>
            }
          />
        )}
        <Pressable onPress={() => router.back()} style={styles.back}>
          <ThemedText type="code">Back</ThemedText>
        </Pressable>
      </SafeAreaView>
    </ThemedView>
  );
}

function RunRow({ run }: { run: ScheduleRun }) {
  const ok = run.outcome === RunOutcome.Ok;
  return (
    <View style={styles.row}>
      <View style={[styles.dot, { backgroundColor: ok ? '#3FB950' : '#F85149' }]} />
      <View style={styles.rowText}>
        <ThemedText numberOfLines={1}>{formatFired(run.firedAt)}</ThemedText>
        {/* On success the session id is the only extra detail; on failure the
            desktop's short note is what tells the user what went wrong. */}
        {run.detail ? (
          <ThemedText type="small" style={styles.muted} numberOfLines={2}>
            {run.detail}
          </ThemedText>
        ) : run.sessionId ? (
          <ThemedText type="small" style={styles.muted} numberOfLines={1}>
            Session {run.sessionId}
          </ThemedText>
        ) : null}
      </View>
      <ThemedText type="small" style={[styles.badge, { color: ok ? '#3FB950' : '#F85149' }]}>
        {ok ? 'Ran' : 'Failed'}
      </ThemedText>
    </View>
  );
}

/** The scheduled instant in the reader's own locale and zone. */
function formatFired(rfc3339: string): string {
  const when = new Date(rfc3339);
  if (Number.isNaN(when.getTime())) return rfc3339;
  return when.toLocaleString(undefined, {
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  });
}

const styles = StyleSheet.create({
  fill: { flex: 1 },
  centre: { flex: 1, alignItems: 'center', justifyContent: 'center', padding: Spacing.four },
  list: { flexGrow: 1, paddingHorizontal: Spacing.four, paddingVertical: Spacing.three, gap: Spacing.three },
  row: { flexDirection: 'row', alignItems: 'center', gap: Spacing.three },
  dot: { width: 8, height: 8, borderRadius: 4 },
  rowText: { flex: 1, gap: Spacing.half },
  muted: { opacity: 0.7 },
  badge: { fontVariant: ['tabular-nums'] },
  empty: { opacity: 0.7, textAlign: 'center' },
  error: { color: '#F85149', paddingHorizontal: Spacing.three, paddingVertical: Spacing.two },
  back: { alignSelf: 'center', paddingVertical: Spacing.three },
});
