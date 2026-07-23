import { useMemo, useState } from 'react';
import { Pressable, StyleSheet, View } from 'react-native';

import { DiffView } from '@/components/git/diff-view';
import { ThemedText } from '@/components/themed-text';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import type { TurnFileChange } from '@/native/thread';
import { parseUnifiedDiff } from '@/native/unified-diff';

/**
 * What a turn changed on disk: a stats header, and — when the backend reported a
 * unified diff — the actual hunks behind a tap.
 *
 * Rendering goes through the git screen's [`DiffView`] rather than a second
 * implementation. A turn diff and a working-tree diff differ in where they came
 * from, not in what they look like, so two renderers would be duplication that
 * drifts.
 *
 * The diff is parsed **on expand, not on mount**. A turn can carry a
 * thousand-line diff, and several such entries may sit in one transcript; paying
 * to parse them all while they are collapsed would make scrolling pay for
 * content nobody has asked to see.
 */
export function TurnDiffCard({ files, diff }: { files: TurnFileChange[]; diff: string | null }) {
  const theme = useTheme();
  const [expanded, setExpanded] = useState(false);

  const added = files.reduce((n, f) => n + f.added, 0);
  const removed = files.reduce((n, f) => n + f.removed, 0);

  // Keyed on `expanded` so the work happens on the first tap and is then cached
  // for later collapse/expand cycles.
  const parsed = useMemo(
    () => (expanded && diff ? parseUnifiedDiff(diff) : []),
    [expanded, diff]
  );

  return (
    <View style={[styles.card, { backgroundColor: theme.backgroundElement }]}>
      <Pressable onPress={() => setExpanded((v) => !v)} style={styles.header}>
        <ThemedText type="small" style={styles.headerText}>
          {expanded ? '▾' : '▸'}{' '}
          {files.length === 1 ? '1 file changed' : `${files.length} files changed`}
          {'  '}
          <ThemedText type="small" themeColor="diffAdd">
            +{added}
          </ThemedText>{' '}
          <ThemedText type="small" themeColor="diffRemove">
            −{removed}
          </ThemedText>
        </ThemedText>
      </Pressable>

      {!expanded ? (
        files.slice(0, FILES_COLLAPSED).map((f) => (
          <ThemedText key={f.path} type="code" numberOfLines={1} style={styles.muted}>
            {f.path}
          </ThemedText>
        ))
      ) : (
        <ExpandedBody files={files} parsed={parsed} hasDiff={Boolean(diff)} />
      )}

      {!expanded && files.length > FILES_COLLAPSED ? (
        <ThemedText type="small" style={styles.muted}>
          …and {files.length - FILES_COLLAPSED} more
        </ThemedText>
      ) : null}
    </View>
  );
}

function ExpandedBody({
  files,
  parsed,
  hasDiff,
}: {
  files: TurnFileChange[];
  parsed: ReturnType<typeof parseUnifiedDiff>;
  hasDiff: boolean;
}) {
  if (hasDiff && parsed.length > 0) {
    return (
      <View style={styles.diffs}>
        {parsed.map((file, i) => (
          <DiffView key={`${i}-${file.path}`} file={file} />
        ))}
      </View>
    );
  }

  // Not a degraded state to paper over: Claude/ACP/Pi genuinely do not report a
  // per-turn diff, so an empty body here would read as a bug or a dead tap. Say
  // what happened and point at the place the information does exist.
  return (
    <View style={styles.diffs}>
      {files.map((f) => (
        <ThemedText key={f.path} type="code" numberOfLines={1} style={styles.fileRow}>
          {f.path}
          {'  '}
          <ThemedText type="code" themeColor="diffAdd">
            +{f.added}
          </ThemedText>{' '}
          <ThemedText type="code" themeColor="diffRemove">
            −{f.removed}
          </ThemedText>
        </ThemedText>
      ))}
      <ThemedText type="small" style={styles.muted}>
        {hasDiff
          ? 'The reported diff could not be read.'
          : "This agent doesn't report line-by-line changes for a turn — open the Git tab to see the current diff."}
      </ThemedText>
    </View>
  );
}

/** File rows shown before the card is expanded. */
const FILES_COLLAPSED = 8;

const styles = StyleSheet.create({
  card: { borderRadius: Spacing.two, padding: Spacing.three, gap: Spacing.two },
  header: { flexDirection: 'row', alignItems: 'center' },
  headerText: { flex: 1 },
  diffs: { gap: Spacing.three },
  fileRow: { opacity: 0.9 },
  muted: { opacity: 0.7 },
});
