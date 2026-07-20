import { router } from 'expo-router';
import type { SessionSummary } from 'oximux-core';
import { useCallback, useEffect, useState } from 'react';
import { FlatList, Pressable, RefreshControl, StyleSheet, View } from 'react-native';
import { SafeAreaView } from 'react-native-safe-area-context';

import { ConnectionBadge } from '@/components/connection-badge';
import { ThemedText } from '@/components/themed-text';
import { ThemedView } from '@/components/themed-view';
import { Spacing } from '@/constants/theme';
import { useClient } from '@/native/client';

export default function SessionsScreen() {
  const sessions = useClient((s) => s.sessions);
  const phase = useClient((s) => s.phase);
  const refreshSessions = useClient((s) => s.refreshSessions);
  const [refreshing, setRefreshing] = useState(false);

  const refresh = useCallback(async () => {
    setRefreshing(true);
    try {
      await refreshSessions();
    } finally {
      setRefreshing(false);
    }
  }, [refreshSessions]);

  // Re-list whenever the link comes back: the desktop may have opened or closed
  // sessions while the phone was away, and nothing pushes that as an event.
  useEffect(() => {
    if (phase === 'connected') void refreshSessions();
  }, [phase, refreshSessions]);

  return (
    <ThemedView style={styles.fill}>
      <SafeAreaView style={styles.fill} edges={['bottom']}>
        <View style={styles.header}>
          <ConnectionBadge />
        </View>
        <FlatList
          data={sessions}
          keyExtractor={(s) => s.sessionId}
          renderItem={({ item }) => <SessionRow session={item} />}
          contentContainerStyle={styles.list}
          refreshControl={<RefreshControl refreshing={refreshing} onRefresh={refresh} />}
          ListEmptyComponent={
            <ThemedText type="small" style={styles.empty}>
              {phase === 'connected'
                ? 'No agent sessions open on the desktop.'
                : 'Waiting for the host…'}
            </ThemedText>
          }
        />
      </SafeAreaView>
    </ThemedView>
  );
}

function SessionRow({ session }: { session: SessionSummary }) {
  return (
    <Pressable
      onPress={() =>
        router.push({ pathname: '/session/[id]', params: { id: session.sessionId } })
      }
      style={styles.row}
    >
      <View style={styles.rowText}>
        <ThemedText numberOfLines={1}>{session.title}</ThemedText>
        {session.model ? (
          <ThemedText type="small" numberOfLines={1}>
            {session.model}
          </ThemedText>
        ) : null}
      </View>
      {/* A session blocked on a permission is the one thing worth crossing the
          room for, so it gets the only accent in the row. */}
      {session.awaitingPermission ? <View style={styles.attention} /> : null}
    </Pressable>
  );
}

const styles = StyleSheet.create({
  fill: { flex: 1 },
  header: { paddingHorizontal: Spacing.four, paddingVertical: Spacing.three },
  list: { paddingHorizontal: Spacing.four, gap: Spacing.three },
  row: { flexDirection: 'row', alignItems: 'center', gap: Spacing.three, paddingVertical: Spacing.two },
  rowText: { flex: 1, gap: Spacing.half },
  attention: { width: 8, height: 8, borderRadius: 4, backgroundColor: '#F5A623' },
  empty: { textAlign: 'center', paddingTop: Spacing.five },
});
