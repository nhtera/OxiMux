import { Stack, router } from 'expo-router';
import { Plus } from 'lucide-react-native';
import { useCallback, useState } from 'react';
import { Alert, FlatList, Pressable, RefreshControl, StyleSheet } from 'react-native';
import { SafeAreaView } from 'react-native-safe-area-context';

import { ConnectionBanner } from '@/components/connection-banner';
import { ScheduleFormSheet } from '@/components/schedules/schedule-form-sheet';
import { ScheduleRow } from '@/components/schedules/schedule-row';
import { ThemedText } from '@/components/themed-text';
import { ThemedView } from '@/components/themed-view';
import { EmptyState } from '@/components/ui/empty-state';
import { ErrorBanner } from '@/components/ui/error-banner';
import { SkeletonList } from '@/components/ui/skeleton';
import { impact, tick } from '@/native/haptics';
import { Icon } from '@/components/ui/icon';
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
        {
          text: 'Delete',
          style: 'destructive',
          onPress: () => {
            impact();
            void remove(id);
          },
        },
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
            <Pressable
              onPress={() => setCreating(true)}
              accessibilityLabel="New schedule"
              style={styles.headerAction}
            >
              <Icon icon={Plus} size="sm" />
              <ThemedText type="code">New</ThemedText>
            </Pressable>
          ),
        }}
      />
      <SafeAreaView style={styles.fill} edges={['bottom']}>
        <ConnectionBanner />
        {error ? <ErrorBanner message={error} /> : null}

        {loading && schedules.length === 0 ? (
          <SkeletonList />
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
                onToggle={(enabled) => {
                  tick();
                  void toggle(item.id, enabled);
                }}
                onDelete={() => confirmDelete(item.id, item.name)}
              />
            )}
            refreshControl={<RefreshControl refreshing={loading} onRefresh={refresh} />}
            ListEmptyComponent={
              <EmptyState
                title="No schedules yet."
                message="Tap New to have the desktop run an agent on a recurring time."
              />
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
  list: { flexGrow: 1, paddingHorizontal: Spacing.four, paddingVertical: Spacing.three, gap: Spacing.two },
  headerAction: { flexDirection: 'row', alignItems: 'center', gap: Spacing.one },
});
