import { Stack, useLocalSearchParams } from 'expo-router';
import { useCallback, useEffect, useState } from 'react';
import { ActivityIndicator, Pressable, RefreshControl, ScrollView, StyleSheet, TextInput, View } from 'react-native';
import { SafeAreaView } from 'react-native-safe-area-context';

import { DiffView } from '@/components/git/diff-view';
import { ThemedText } from '@/components/themed-text';
import { ThemedView } from '@/components/themed-view';
import { Spacing } from '@/constants/theme';
import { useClient } from '@/native/client';
import { describeError } from '@/native/errors';
import {
  fileLabel,
  fileLines,
  isStaged,
  isUntracked,
  parseGit,
  type FileDiff,
  type GitFile,
  type GitStatus,
} from '@/native/git';
import { useTheme } from '@/hooks/use-theme';

/**
 * Git for the repository a session lives in. The host resolves the repo from the
 * session's own cwd and re-contains every path, so this screen never sends an
 * absolute path or reasons about where the repo is.
 */
export default function GitScreen() {
  const { id } = useLocalSearchParams<{ id: string }>();
  const sessionId = String(id);
  const client = useClient((s) => s.client);
  const [status, setStatus] = useState<GitStatus>();
  const [openPath, setOpenPath] = useState<string>();
  const [diff, setDiff] = useState<FileDiff[]>();
  const [message, setMessage] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();

  const refresh = useCallback(async () => {
    // Say so rather than returning quietly: `status` would stay undefined and
    // the screen would sit on a spinner forever, looking like it is still
    // loading when nothing is in flight.
    if (!client) {
      setError('Not connected to the desktop.');
      return;
    }
    try {
      setError(undefined);
      setStatus(parseGit<GitStatus>(await client.gitStatus(sessionId)));
    } catch (e) {
      setError(describeError(e));
    }
  }, [client, sessionId]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const openDiff = useCallback(
    async (file: GitFile) => {
      if (!client) {
        setError('Not connected to the desktop.');
        return;
      }
      if (openPath === file.path) {
        setOpenPath(undefined);
        setDiff(undefined);
        return;
      }
      setOpenPath(file.path);
      setDiff(undefined);
      try {
        const json = await client.gitDiff(
          sessionId,
          file.path,
          isStaged(file),
          isUntracked(file)
        );
        setDiff(parseGit<FileDiff[]>(json));
      } catch (e) {
        setError(describeError(e));
      }
    },
    [client, openPath, sessionId]
  );

  /**
   * Run a write, then re-read status so the rows reflect what actually landed.
   * The client is handed to the action rather than asserted non-null at each
   * call site: it genuinely can be absent (a drop between render and tap), and a
   * `!` there would surface as "cannot read properties of undefined" instead of
   * something the user can act on.
   */
  const write = useCallback(
    async (action: (c: NonNullable<typeof client>) => Promise<unknown>) => {
      if (busy) return;
      const current = client;
      if (!current) {
        setError('Not connected to the desktop.');
        return;
      }
      setBusy(true);
      try {
        setError(undefined);
        await action(current);
        await refresh();
      } catch (e) {
        setError(describeError(e));
      } finally {
        setBusy(false);
      }
    },
    [busy, client, refresh]
  );

  const staged = status?.files.filter(isStaged) ?? [];

  return (
    <ThemedView style={styles.fill}>
      <Stack.Screen options={{ title: status?.branch ?? 'Git' }} />
      <SafeAreaView style={styles.fill} edges={['bottom']}>
        <ScrollView
          contentContainerStyle={styles.list}
          refreshControl={<RefreshControl refreshing={false} onRefresh={refresh} />}
        >
          {status ? (
            <ThemedText type="small" style={styles.muted}>
              {status.branch ?? 'detached HEAD'}
              {status.upstream ? ` → ${status.upstream}` : ''}
              {status.ahead > 0 ? ` · ${status.ahead} ahead` : ''}
              {status.behind > 0 ? ` · ${status.behind} behind` : ''}
            </ThemedText>
          ) : error ? null : (
            // Only spin while the status is genuinely still in flight — a failed
            // load leaves `status` undefined too, and a spinner above an error
            // message reads as "still trying" when nothing is.
            <ActivityIndicator />
          )}

          {error ? (
            <ThemedText type="small" style={styles.error}>
              {error}
            </ThemedText>
          ) : null}

          {status?.files.length === 0 ? (
            <ThemedText type="small" style={styles.muted}>
              Working tree clean.
            </ThemedText>
          ) : null}

          {status?.files.map((file) => (
            <View key={file.path} style={styles.fileBlock}>
              <FileRow
                file={file}
                busy={busy}
                open={openPath === file.path}
                onOpen={() => openDiff(file)}
                onToggleStage={() =>
                  write((c) =>
                    isStaged(file)
                      ? c.gitUnstage(sessionId, [file.path])
                      : c.gitStage(sessionId, [file.path])
                  )
                }
              />
              {openPath === file.path ? (
                diff ? (
                  diff.map((d) => <DiffView key={d.path} file={d} />)
                ) : (
                  <ActivityIndicator />
                )
              ) : null}
            </View>
          ))}
        </ScrollView>

        <CommitBar
          stagedCount={staged.length}
          message={message}
          busy={busy}
          onChangeMessage={setMessage}
          onCommit={() =>
            write(async (c) => {
              await c.gitCommit(sessionId, message.trim());
              setMessage('');
            })
          }
        />
      </SafeAreaView>
    </ThemedView>
  );
}

function FileRow({
  file,
  open,
  busy,
  onOpen,
  onToggleStage,
}: {
  file: GitFile;
  open: boolean;
  busy: boolean;
  onOpen: () => void;
  onToggleStage: () => void;
}) {
  const [added, removed] = fileLines(file);
  return (
    <View style={styles.row}>
      <Pressable onPress={onOpen} style={styles.rowMain}>
        <ThemedText type="code" numberOfLines={1}>
          {open ? '▾ ' : '▸ '}
          {file.path}
        </ThemedText>
        <ThemedText type="small" style={styles.muted}>
          {fileLabel(file)}
          {added || removed ? (
            <>
              {'  '}
              <ThemedText type="small" style={styles.added}>
                +{added}
              </ThemedText>{' '}
              <ThemedText type="small" style={styles.removed}>
                −{removed}
              </ThemedText>
            </>
          ) : null}
        </ThemedText>
      </Pressable>

      <Pressable disabled={busy} onPress={onToggleStage} style={[styles.stage, busy && styles.dim]}>
        <ThemedText type="small">{isStaged(file) ? 'Unstage' : 'Stage'}</ThemedText>
      </Pressable>
    </View>
  );
}

function CommitBar({
  stagedCount,
  message,
  busy,
  onChangeMessage,
  onCommit,
}: {
  stagedCount: number;
  message: string;
  busy: boolean;
  onChangeMessage: (v: string) => void;
  onCommit: () => void;
}) {
  const theme = useTheme();
  // Commit is staged-only by design on the host — it never pre-stages with
  // `git add`, because that would flatten hunk-level partial staging set up on
  // the desktop that this screen cannot see. So with nothing staged there is
  // genuinely nothing to commit, and the button says so rather than failing.
  const canCommit = stagedCount > 0 && message.trim().length > 0 && !busy;
  return (
    <View style={[styles.commitBar, { borderTopColor: theme.backgroundSelected }]}>
      <TextInput
        value={message}
        onChangeText={onChangeMessage}
        placeholder={stagedCount > 0 ? 'Commit message…' : 'Stage something to commit'}
        placeholderTextColor={theme.textSecondary}
        style={[styles.input, { backgroundColor: theme.backgroundElement, color: theme.text }]}
        multiline
      />
      <Pressable
        onPress={onCommit}
        disabled={!canCommit}
        style={[
          styles.commitButton,
          { backgroundColor: theme.backgroundSelected },
          !canCommit && styles.dim,
        ]}
      >
        <ThemedText type="code">
          Commit{stagedCount > 0 ? ` ${stagedCount}` : ''}
        </ThemedText>
      </Pressable>
    </View>
  );
}

const styles = StyleSheet.create({
  fill: { flex: 1 },
  list: { padding: Spacing.three, gap: Spacing.three },
  fileBlock: { gap: Spacing.two },
  row: { flexDirection: 'row', alignItems: 'center', gap: Spacing.two },
  rowMain: { flex: 1, gap: Spacing.half },
  stage: {
    paddingVertical: Spacing.one,
    paddingHorizontal: Spacing.two,
    borderWidth: 1,
    borderColor: '#8A8F98',
    borderRadius: Spacing.one,
  },
  commitBar: {
    borderTopWidth: StyleSheet.hairlineWidth,
    padding: Spacing.three,
    gap: Spacing.two,
  },
  input: {
    minHeight: 44,
    maxHeight: 120,
    borderRadius: Spacing.two,
    paddingHorizontal: Spacing.three,
    paddingVertical: Spacing.two,
    fontSize: 16,
  },
  commitButton: {
    alignSelf: 'flex-end',
    paddingVertical: Spacing.two,
    paddingHorizontal: Spacing.four,
    borderRadius: Spacing.two,
  },
  muted: { opacity: 0.7 },
  dim: { opacity: 0.4 },
  added: { color: '#3FB950' },
  removed: { color: '#F85149' },
  error: { color: '#F85149' },
});
