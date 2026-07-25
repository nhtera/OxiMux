import { Stack, router } from 'expo-router';
import { Plus, Search } from 'lucide-react-native';
import { useCallback, useEffect, useMemo, useState } from 'react';
import { RefreshControl, SectionList, StyleSheet, TextInput, View } from 'react-native';
import { SafeAreaView } from 'react-native-safe-area-context';

import { CommandPalette } from '@/components/command-palette';
import { ConnectionBanner } from '@/components/connection-banner';
import { NewSessionSheet } from '@/components/new-session-sheet';
import { ProjectGroupHeader, PROJECT_INDENT } from '@/components/sessions/project-group-header';
import { SessionListRow } from '@/components/sessions/session-list-row';
import { ThemedText } from '@/components/themed-text';
import { ThemedView } from '@/components/themed-view';
import { EmptyState } from '@/components/ui/empty-state';
import { IconButton } from '@/components/ui/icon-button';
import { SkeletonList } from '@/components/ui/skeleton';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { useClient } from '@/native/client';
import { describeError } from '@/native/errors';
import { filterSessions } from '@/native/session-filter';
import { groupSessionsByProject } from '@/native/session-grouping';
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

  // Which projects the user has folded open or shut, by key. Absent means "not
  // touched", which is why the default is computed rather than seeded: a project
  // that gains its first session should open on its own, and one that arrives
  // later must not inherit a neighbour's state.
  const [toggled, setToggled] = useState<Record<string, boolean>>({});
  // The rows animate themselves in and out (see `SessionListRow`) rather than
  // this configuring `LayoutAnimation` here: frame-by-frame capture of a toggle
  // showed that API changing nothing at all under the New Architecture — the
  // rows appeared between two frames — while the row-level transition measures
  // as a six-frame fade in both directions.
  const toggle = useCallback(
    (key: string, expanded: boolean) => setToggled((t) => ({ ...t, [key]: !expanded })),
    []
  );
  // `data` is what collapsing acts on — an empty section still renders its
  // header, so the list keeps every project reachable while hiding its rows.
  // `count` survives that emptying, since the header reports it while shut.
  const sections = useMemo(
    () =>
      groups.map((g) => {
        const expanded = toggled[g.key] ?? g.data.length > 0;
        return { ...g, expanded, count: g.data.length, data: expanded ? g.data : [] };
      }),
    [groups, toggled]
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
            sections={sections}
            keyExtractor={(s) => s.sessionId}
            renderItem={({ item, section }) => (
              // The project sits in the section header, so hide the per-row project
              // label there; a pathless group (flat / "Other") has no header, so it
              // keeps rendering the project inline.
              <SessionListRow session={item} showProject={!section.path} />
            )}
            renderSectionHeader={({ section }) =>
              section.name === '' ? null : (
                <ProjectGroupHeader
                  name={section.name}
                  count={section.count}
                  expanded={section.expanded}
                  onToggle={() => toggle(section.key, section.expanded)}
                  // Only a real project (with a path) can host a new session; the
                  // "Other" bucket has none, so it gets no compose affordance.
                  onCompose={section.path ? () => openNewSession(section.path as string) : undefined}
                />
              )
            }
            // An open project with nothing in it says so, rather than showing a
            // name with a gap under it — the state that used to look like a list
            // that had failed to load.
            renderSectionFooter={({ section }) =>
              section.expanded && section.count === 0 && section.name !== '' ? (
                <ThemedText type="small" themeColor="textMuted" style={styles.emptyProject}>
                  No sessions yet
                </ThemedText>
              ) : null
            }
            // No `ItemSeparatorComponent`: indentation groups the rows now, and
            // the rule it drew was inset on the left but ran to the screen edge
            // on the right.
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

const styles = StyleSheet.create({
  fill: { flex: 1 },
  headerActions: { flexDirection: 'row', alignItems: 'center', gap: Spacing.two },
  // `flexGrow` is load-bearing, not cosmetic: without it the content container
  // collapses to the height of the empty-state text, leaving nothing tall enough
  // to drag — so pull-to-refresh silently does nothing in exactly the state where
  // it is needed most, and an empty list looks like a host exposing no sessions.
  list: { flexGrow: 1, paddingBottom: Spacing.four },
  // Aligned with the session rows it stands in for, so an empty project reads as
  // the same column of content rather than a stray caption.
  emptyProject: {
    paddingLeft: Spacing.four + PROJECT_INDENT,
    paddingRight: Spacing.four,
    paddingVertical: Spacing.two,
  },
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
