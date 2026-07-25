import { Stack, router } from 'expo-router';
import { Plus, Search, SquarePen } from 'lucide-react-native';
import type { SessionSummary } from 'oximux-core';
import { useCallback, useEffect, useMemo, useState } from 'react';
import { Pressable, RefreshControl, SectionList, StyleSheet, TextInput, View } from 'react-native';
import { SafeAreaView } from 'react-native-safe-area-context';

import { CommandPalette } from '@/components/command-palette';
import { ConnectionBanner } from '@/components/connection-banner';
import { NewSessionSheet } from '@/components/new-session-sheet';
import { ThemedText } from '@/components/themed-text';
import { ThemedView } from '@/components/themed-view';
import { EmptyState } from '@/components/ui/empty-state';
import { Icon } from '@/components/ui/icon';
import { IconButton } from '@/components/ui/icon-button';
import { ListDivider } from '@/components/ui/list-row';
import { SkeletonList } from '@/components/ui/skeleton';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { useClient } from '@/native/client';
import { describeError } from '@/native/errors';
import { tick } from '@/native/haptics';
import { filterSessions } from '@/native/session-filter';
import { groupSessionsByProject } from '@/native/session-grouping';
import { parseSessionTitle } from '@/native/session-title';
import { useNewSessionIntent } from '@/stores/new-session-intent';

export default function SessionsScreen() {
  const sessions = useClient((s) => s.sessions);
  const projects = useClient((s) => s.projects);
  const phase = useClient((s) => s.phase);
  const cause = useClient((s) => s.cause);
  const refreshSessions = useClient((s) => s.refreshSessions);
  const ensureConnected = useClient((s) => s.ensureConnected);
  const [refreshing, setRefreshing] = useState(false);
  const [retrying, setRetrying] = useState(false);
  const [query, setQuery] = useState('');
  const theme = useTheme();
  const client = useClient((s) => s.client);
  // The drawer's Sessions "+" raises a one-shot intent rather than opening the
  // sheet itself (the sheet lives here). If it was raised before this screen
  // mounted (drawer tapped from another screen), open with the sheet already up.
  const [creating, setCreating] = useState(() => useNewSessionIntent.getState().requested);
  const [busy, setBusy] = useState(false);
  const [createError, setCreateError] = useState<string>();
  // Set when the sheet is opened from a project's compose "+": it pre-fills the
  // project's path so the user confirms (and sees any error) rather than typing —
  // undefined for a plain "New session", which opens blank.
  const [prefillCwd, setPrefillCwd] = useState<string>();
  const [paletteOpen, setPaletteOpen] = useState(false);

  const openNewSession = useCallback((cwd?: string) => {
    setPrefillCwd(cwd);
    setCreating(true);
  }, []);

  // Consume the intent honored by the initial state above, and open the sheet for
  // an intent raised while this screen is already mounted. Subscribing keeps the
  // setState out of the effect body (the create is an external-event callback).
  useEffect(() => {
    const intent = useNewSessionIntent.getState();
    if (intent.requested) intent.consume();
    return useNewSessionIntent.subscribe((s) => {
      if (s.requested) {
        setPrefillCwd(undefined);
        setCreating(true);
        useNewSessionIntent.getState().consume();
      }
    });
  }, []);

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

  // Group by project for the sectioned list — but while filtering, drop the
  // project headers and show a flat list of matches (an empty project header
  // amid search results is noise). `groupSessionsByProject([], …)` yields one
  // pathless group, which renders headerless.
  const groups = useMemo(
    () => groupSessionsByProject(filtering ? [] : projects, visible),
    [filtering, projects, visible]
  );
  // Nothing to list when there are no matches and no project headers to offer —
  // that is when the skeleton / empty / down message takes over. A connected host
  // with projects but no sessions still lists (the per-project "+" starts one).
  const nothingToList = visible.length === 0 && (filtering || projects.length === 0);

  const refresh = useCallback(async () => {
    setRefreshing(true);
    try {
      // Reconnect first if the link is down — pull-to-refresh is the gesture a
      // user reaches for when a screen looks stale, so it must recover the
      // connection, not just re-list against a dead one. A no-op when live.
      await ensureConnected();
      await refreshSessions();
    } finally {
      setRefreshing(false);
    }
  }, [ensureConnected, refreshSessions]);

  const retry = useCallback(async () => {
    setRetrying(true);
    try {
      await ensureConnected();
    } finally {
      setRetrying(false);
    }
  }, [ensureConnected]);

  // Down states (no live link, no dial in flight): the reconnect driver has
  // given up, so offer an explicit retry alongside the foreground auto-reconnect.
  const down = phase === 'idle' || phase === 'disconnected' || phase === 'unreachable';

  // The session list is kept live by a host push subscription registered in the
  // client store (the desktop streams open/close/rename/permission changes), so
  // there is nothing to poll here. Pull-to-refresh remains as a manual nudge, and
  // a foreground reconnect (wired at the app root) revives a link the OS dropped.

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
              <IconButton icon={Plus} accessibilityLabel="New session" onPress={() => openNewSession()} />
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

        {nothingToList ? (
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
              // While down, the core's failure reason is the only thing that
              // separates "the desktop is off" from "this network cannot reach
              // it" — the difference between waiting and moving networks.
              message={
                filtering ? `No sessions match “${query.trim()}”.` : down ? cause : undefined
              }
              action={
                !filtering && down ? { label: 'Retry', onPress: retry, loading: retrying } : undefined
              }
            />
          )
        ) : (
          <SectionList
            sections={groups}
            keyExtractor={(s) => s.sessionId}
            renderItem={({ item, section }) => (
              // The project sits in the section header, so hide the per-row project
              // label there; a pathless group (flat / "Other") has no header, so it
              // keeps rendering the project inline.
              <SessionRow session={item} showProject={!section.path} />
            )}
            renderSectionHeader={({ section }) =>
              section.name === '' ? null : (
                <View style={[styles.sectionHeader, { backgroundColor: theme.background }]}>
                  <ThemedText type="small" themeColor="textMuted" numberOfLines={1} style={styles.sectionName}>
                    {section.name}
                  </ThemedText>
                  {/* Only a real project (with a path) can host a new session; the
                      "Other" bucket has none, so it gets no compose affordance. */}
                  {section.path ? (
                    <Pressable
                      onPress={() => {
                        tick();
                        openNewSession(section.path as string);
                      }}
                      accessibilityLabel={`New session in ${section.name}`}
                      hitSlop={Spacing.two}
                    >
                      <Icon icon={SquarePen} size="sm" color={theme.textSecondary} />
                    </Pressable>
                  ) : null}
                </View>
              )
            }
            ItemSeparatorComponent={ListDivider}
            stickySectionHeadersEnabled={false}
            contentContainerStyle={styles.list}
            refreshControl={<RefreshControl refreshing={refreshing} onRefresh={refresh} />}
          />
        )}
        <NewSessionSheet
          visible={creating}
          busy={busy}
          error={createError}
          initialCwd={prefillCwd}
          onCreate={create}
          onClose={() => {
            setCreating(false);
            setCreateError(undefined);
            setPrefillCwd(undefined);
          }}
        />
        <CommandPalette
          visible={paletteOpen}
          onClose={() => setPaletteOpen(false)}
          onNewSession={() => openNewSession()}
        />
      </SafeAreaView>
    </ThemedView>
  );
}

function SessionRow({ session, showProject }: { session: SessionSummary; showProject: boolean }) {
  const theme = useTheme();
  // The host folds the project into the title so a row is attributable to its
  // project without a wire-schema change; render it as a muted line above — but
  // only when the row is not already under a project section header.
  const { project, label } = parseSessionTitle(session.title);
  return (
    <Pressable
      onPress={() => {
        tick();
        router.push({ pathname: '/session/[id]', params: { id: session.sessionId } });
      }}
      style={({ pressed }) => [styles.row, pressed && { backgroundColor: theme.surface2 }]}
    >
      <View style={styles.rowText}>
        {showProject && project ? (
          <ThemedText type="small" numberOfLines={1} style={{ color: theme.textSecondary }}>
            {project}
          </ThemedText>
        ) : null}
        <ThemedText numberOfLines={1}>{label}</ThemedText>
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
  sectionHeader: {
    flexDirection: 'row',
    alignItems: 'center',
    justifyContent: 'space-between',
    gap: Spacing.three,
    paddingHorizontal: Spacing.four,
    paddingTop: Spacing.four,
    paddingBottom: Spacing.two,
  },
  sectionName: { flex: 1, textTransform: 'uppercase', letterSpacing: 0.5 },
  search: {
    marginHorizontal: Spacing.four,
    // Symmetric top gap: the connection banner above is absent while connected,
    // so without this the field butts straight against the header.
    marginTop: Spacing.three,
    marginBottom: Spacing.three,
    borderRadius: Spacing.two,
    paddingHorizontal: Spacing.three,
    paddingVertical: Spacing.two,
    fontSize: 16,
  },
});
