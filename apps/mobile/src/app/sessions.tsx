import { Stack, router, useFocusEffect } from 'expo-router';
import { Plus, Search } from 'lucide-react-native';
import type { SessionSummary } from 'oximux-core';
import { useCallback, useState } from 'react';
import { AppState, FlatList, Pressable, RefreshControl, StyleSheet, TextInput, View } from 'react-native';
import { SafeAreaView } from 'react-native-safe-area-context';

import { CommandPalette } from '@/components/command-palette';
import { ConnectionBanner } from '@/components/connection-banner';
import { NewSessionSheet } from '@/components/new-session-sheet';
import { ThemedText } from '@/components/themed-text';
import { ThemedView } from '@/components/themed-view';
import { EmptyState } from '@/components/ui/empty-state';
import { IconButton } from '@/components/ui/icon-button';
import { ListDivider } from '@/components/ui/list-row';
import { SkeletonList } from '@/components/ui/skeleton';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { useClient } from '@/native/client';
import { describeError } from '@/native/errors';
import { tick } from '@/native/haptics';
import { filterSessions } from '@/native/session-filter';

/** How often to re-list while the Sessions screen is focused and connected —
 *  frequent enough to feel live, cheap enough for a list RPC. */
const SESSION_POLL_MS = 5000;

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
  const [paletteOpen, setPaletteOpen] = useState(false);

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

  // The host never pushes session open/close/rename, so the list would otherwise
  // drift the whole time the phone stays connected. Keep it live while this screen
  // is on top: re-list on focus, poll on a short interval, and re-list whenever the
  // app returns to the foreground. The interval is torn down when the screen blurs
  // (a session is opened on top) so a backgrounded list isn't polling. Pull-to-
  // refresh stays for an explicit nudge.
  useFocusEffect(
    useCallback(() => {
      if (phase !== 'connected') return;
      void refreshSessions();
      const id = setInterval(() => void refreshSessions(), SESSION_POLL_MS);
      const sub = AppState.addEventListener('change', (next) => {
        if (next === 'active') void refreshSessions();
      });
      return () => {
        clearInterval(id);
        sub.remove();
      };
    }, [phase, refreshSessions])
  );

  return (
    <ThemedView style={styles.fill}>
      <Stack.Screen
        options={{
          title: 'Sessions',
          // Search + New sit in the header; the primary destinations moved into the
          // nav drawer (opened by the header menu button), retiring the row of text
          // buttons that used to double up beneath the native title.
          headerRight: () => (
            <View style={styles.headerActions}>
              <IconButton icon={Search} accessibilityLabel="Search and jump" onPress={() => setPaletteOpen(true)} />
              <IconButton icon={Plus} accessibilityLabel="New session" onPress={() => setCreating(true)} />
            </View>
          ),
        }}
      />
      <SafeAreaView style={styles.fill} edges={['bottom']}>
        <ConnectionBanner />
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
          ItemSeparatorComponent={ListDivider}
          contentContainerStyle={styles.list}
          refreshControl={<RefreshControl refreshing={refreshing} onRefresh={refresh} />}
          ListEmptyComponent={
            // While the first link is still coming up there is nothing to say yet,
            // so a skeleton reads as "content arriving" rather than a false "no
            // sessions". Once connected (or failed), fall back to the real message.
            // Kept distinct from the no-sessions state on purpose: telling a user
            // whose filter matched nothing that the desktop has no sessions open
            // would be a lie about the desktop.
            !filtering && (phase === 'connecting' || phase === 'reconnecting') ? (
              <SkeletonList />
            ) : (
              <EmptyState
                title={
                  filtering
                    ? 'No matches'
                    : phase === 'connected'
                      ? 'No agent sessions open on the desktop.'
                      : 'Waiting for the host…'
                }
                message={filtering ? `No sessions match “${query.trim()}”.` : undefined}
              />
            )
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
        <CommandPalette
          visible={paletteOpen}
          onClose={() => setPaletteOpen(false)}
          onNewSession={() => setCreating(true)}
        />
      </SafeAreaView>
    </ThemedView>
  );
}

function SessionRow({ session }: { session: SessionSummary }) {
  const theme = useTheme();
  return (
    <Pressable
      onPress={() => {
        tick();
        router.push({ pathname: '/session/[id]', params: { id: session.sessionId } });
      }}
      style={({ pressed }) => [styles.row, pressed && { backgroundColor: theme.surface2 }]}
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
      {session.awaitingPermission ? (
        <View style={[styles.attention, { backgroundColor: theme.warning }]} />
      ) : null}
    </Pressable>
  );
}

const styles = StyleSheet.create({
  fill: { flex: 1 },
  headerActions: { flexDirection: 'row', alignItems: 'center', gap: Spacing.two },
  // `flexGrow` is load-bearing, not cosmetic: without it the content container
  // collapses to the height of the empty-state text, leaving nothing tall enough
  // to drag — so pull-to-refresh silently does nothing in exactly the state where
  // it is needed most, and an empty list looks like a host exposing no sessions.
  list: { flexGrow: 1, paddingVertical: Spacing.two },
  row: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: Spacing.three,
    paddingVertical: Spacing.three,
    paddingHorizontal: Spacing.four,
  },
  rowText: { flex: 1, gap: Spacing.half },
  attention: { width: 8, height: 8, borderRadius: 4 },
  search: {
    marginHorizontal: Spacing.four,
    marginBottom: Spacing.three,
    borderRadius: Spacing.two,
    paddingHorizontal: Spacing.three,
    paddingVertical: Spacing.two,
    fontSize: 16,
  },
});
