import { Link, router } from 'expo-router';
import type { SessionSummary } from 'oximux-core';
import { useCallback, useEffect, useState } from 'react';
import { FlatList, Pressable, RefreshControl, StyleSheet, TextInput, View } from 'react-native';
import { SafeAreaView } from 'react-native-safe-area-context';

import { ConnectionBadge } from '@/components/connection-badge';
import { NewSessionSheet } from '@/components/new-session-sheet';
import { ThemedText } from '@/components/themed-text';
import { ThemedView } from '@/components/themed-view';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { useClient } from '@/native/client';
import { describeError } from '@/native/errors';
import { filterSessions } from '@/native/session-filter';

export default function SessionsScreen() {
  const sessions = useClient((s) => s.sessions);
  const phase = useClient((s) => s.phase);
  const refreshSessions = useClient((s) => s.refreshSessions);
  const [refreshing, setRefreshing] = useState(false);
  const [query, setQuery] = useState('');
  const theme = useTheme();
  const client = useClient((s) => s.client);
  const [creating, setCreating] = useState(false);
  const [busy, setBusy] = useState(false);
  const [createError, setCreateError] = useState<string>();

  const create = useCallback(
    async (cwd: string) => {
      if (!client) return;
      setBusy(true);
      setCreateError(undefined);
      try {
        const sessionId = await client.createSession(cwd, undefined);
        setCreating(false);
        // Straight into the new session rather than back to a list the user then
        // has to find it in — the id comes back from the host precisely so this
        // does not need a refresh-and-guess.
        router.push({ pathname: '/session/[id]', params: { id: sessionId } });
        void refreshSessions();
      } catch (e) {
        setCreateError(describeError(e));
      } finally {
        setBusy(false);
      }
    },
    [client, refreshSessions]
  );

  const visible = filterSessions(sessions, query);
  const filtering = query.trim().length > 0;

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
          {/* The only route off this screen that isn't a session. Without it a
              paired device has no way back to pairing — see settings.tsx. */}
          <View style={styles.headerActions}>
            <Pressable onPress={() => setCreating(true)} accessibilityLabel="New session">
              <ThemedText type="code">New</ThemedText>
            </Pressable>
            <Link href="/terminals">
              <ThemedText type="code">Terminals</ThemedText>
            </Link>
            <Link href="/settings">
              <ThemedText type="code">Settings</ThemedText>
            </Link>
          </View>
        </View>
        {/* Hidden until there is enough to search: a lone session does not need
            a filter, and the box would just be one more thing between the user
            and the list. */}
        {sessions.length > 1 ? (
          <TextInput
            value={query}
            onChangeText={setQuery}
            placeholder="Filter sessions…"
            placeholderTextColor={theme.textSecondary}
            autoCapitalize="none"
            autoCorrect={false}
            clearButtonMode="while-editing"
            style={[
              styles.search,
              { backgroundColor: theme.backgroundElement, color: theme.text },
            ]}
          />
        ) : null}

        <FlatList
          data={visible}
          keyExtractor={(s) => s.sessionId}
          renderItem={({ item }) => <SessionRow session={item} />}
          contentContainerStyle={styles.list}
          refreshControl={<RefreshControl refreshing={refreshing} onRefresh={refresh} />}
          ListEmptyComponent={
            <ThemedText type="small" style={styles.empty}>
              {/* Kept distinct from the no-sessions state on purpose: telling a
                  user whose filter matched nothing that the desktop has no
                  sessions open would be a lie about the desktop. */}
              {filtering
                ? `No sessions match “${query.trim()}”.`
                : phase === 'connected'
                  ? 'No agent sessions open on the desktop.'
                  : 'Waiting for the host…'}
            </ThemedText>
          }
        />
        <NewSessionSheet
          visible={creating}
          busy={busy}
          error={createError}
          onCreate={create}
          onClose={() => {
            setCreating(false);
            setCreateError(undefined);
          }}
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
  headerActions: { flexDirection: 'row', alignItems: 'center', gap: Spacing.three },
  header: {
    flexDirection: 'row',
    alignItems: 'center',
    justifyContent: 'space-between',
    gap: Spacing.three,
    paddingHorizontal: Spacing.four,
    paddingVertical: Spacing.three,
  },
  // `flexGrow` is load-bearing, not cosmetic: without it the content container
  // collapses to the height of the empty-state text, leaving nothing tall enough
  // to drag — so pull-to-refresh silently does nothing in exactly the state where
  // it is needed most, and an empty list looks like a host exposing no sessions.
  list: { flexGrow: 1, paddingHorizontal: Spacing.four, gap: Spacing.three },
  row: { flexDirection: 'row', alignItems: 'center', gap: Spacing.three, paddingVertical: Spacing.two },
  rowText: { flex: 1, gap: Spacing.half },
  attention: { width: 8, height: 8, borderRadius: 4, backgroundColor: '#F5A623' },
  empty: { textAlign: 'center', paddingTop: Spacing.five },
  search: {
    marginHorizontal: Spacing.four,
    marginBottom: Spacing.three,
    borderRadius: Spacing.two,
    paddingHorizontal: Spacing.three,
    paddingVertical: Spacing.two,
    fontSize: 16,
  },
});
