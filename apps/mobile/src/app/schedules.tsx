import { Stack, router } from 'expo-router';
import { useCallback, useState } from 'react';
import {
  ActivityIndicator,
  Alert,
  FlatList,
  Pressable,
  RefreshControl,
  StyleSheet,
  View,
} from 'react-native';
import { SafeAreaView } from 'react-native-safe-area-context';

import { ScheduleFormSheet } from '@/components/schedules/schedule-form-sheet';
import { ScheduleRow } from '@/components/schedules/schedule-row';
import { ThemedText } from '@/components/themed-text';
import { ThemedView } from '@/components/themed-view';
import { Spacing } from '@/constants/theme';
import { describeError } from '@/native/errors';
import { useSchedules, type ScheduleDraft } from '@/native/use-schedules';

/**
 * Scheduled agent runs the desktop holds: create one, pause or delete it, and
 * open its run history.
 *
 * The list is not live — firing schedules is the desktop's job, and they change
 * on human timescales — so this is fetch-on-mount plus pull-to-refresh. Writes
 * update the list from the row the host returns rather than re-listing.
 */
export default function SchedulesScreen() {
  const { schedules, loading, error, refresh, create, toggle, remove } = useSchedules();
  const [creating, setCreating] = useState(false);
  const [busy, setBusy] = useState(false);
  const [createError, setCreateError] = useState<string>();

  // Delete is irreversible and reachable by an easy-to-hit long press, so it
  // confirms. Re-creating a schedule from a phone is enough friction that an
  // accidental deletion is a real annoyance, not a one-tap redo.
  const confirmDelete = useCallback(
    (id: string, name: string) => {
      Alert.alert('Delete schedule?', `“${name}” will stop running. This cannot be undone.`, [
        { text: 'Cancel', style: 'cancel' },
        { text: 'Delete', style: 'destructive', onPress: () => void remove(id) },
      ]);
    },
    [remove],
  );

  const onCreate = useCallback(
    async (draft: ScheduleDraft) => {
      setBusy(true);
      setCreateError(undefined);
      try {
        await create(draft);
        setCreating(false);
      } catch (e) {
        // Kept on the sheet with the user's input intact — a refused create
        // (read-only device, invalid path) is worth showing where it happened.
        setCreateError(describeError(e));
      } finally {
        setBusy(false);
      }
    },
    [create],
  );

  return (
    <ThemedView style={styles.fill}>
      <Stack.Screen
        options={{
          title: 'Schedules',
          headerRight: () => (
            <Pressable onPress={() => setCreating(true)} accessibilityLabel="New schedule">
              <ThemedText type="code">New</ThemedText>
            </Pressable>
          ),
        }}
      />
      <SafeAreaView style={styles.fill} edges={['bottom']}>
        {error ? (
          <ThemedText type="small" style={styles.error} numberOfLines={3}>
            {error}
          </ThemedText>
        ) : null}

        {loading && schedules.length === 0 ? (
          <View style={styles.centre}>
            <ActivityIndicator />
          </View>
        ) : (
          <FlatList
            data={schedules}
            keyExtractor={(s) => s.id}
            contentContainerStyle={styles.list}
            renderItem={({ item }) => (
              <ScheduleRow
                schedule={item}
                onOpen={() =>
                  router.push({ pathname: '/schedules/[id]', params: { id: item.id, name: item.name } })
                }
                onToggle={(enabled) => void toggle(item.id, enabled)}
                onDelete={() => confirmDelete(item.id, item.name)}
              />
            )}
            refreshControl={<RefreshControl refreshing={loading} onRefresh={refresh} />}
            ListEmptyComponent={
              <View style={styles.centre}>
                <ThemedText type="small" style={styles.empty}>
                  No schedules yet. Tap New to have the desktop run an agent on a
                  recurring time.
                </ThemedText>
              </View>
            }
          />
        )}
      </SafeAreaView>

      <ScheduleFormSheet
        visible={creating}
        busy={busy}
        error={createError}
        onCreate={onCreate}
        onClose={() => {
          setCreating(false);
          setCreateError(undefined);
        }}
      />
    </ThemedView>
  );
}

const styles = StyleSheet.create({
  fill: { flex: 1 },
  centre: { flex: 1, alignItems: 'center', justifyContent: 'center', padding: Spacing.four },
  list: { flexGrow: 1, paddingHorizontal: Spacing.four, paddingVertical: Spacing.three, gap: Spacing.two },
  empty: { opacity: 0.7, textAlign: 'center' },
  error: { color: '#F85149', paddingHorizontal: Spacing.three, paddingVertical: Spacing.two },
});
