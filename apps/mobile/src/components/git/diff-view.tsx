import { ScrollView, StyleSheet, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Spacing } from '@/constants/theme';
import type { DiffHunk, DiffLine, FileDiff } from '@/native/git';

const ADDED_BG = 'rgba(63, 185, 80, 0.15)';
const REMOVED_BG = 'rgba(248, 81, 73, 0.15)';

/**
 * A per-file diff, laid out for a phone: no side-by-side, no gutter of line
 * numbers competing with the code for width. Tinted rows carry the +/- meaning
 * instead, and the body scrolls horizontally rather than wrapping — a wrapped
 * diff line loses the column alignment that makes a diff readable.
 */
export function DiffView({ file }: { file: FileDiff }) {
  return (
    <View style={styles.file}>
      <ThemedText type="code" numberOfLines={1} style={styles.path}>
        {file.path}
      </ThemedText>

      {file.large ? (
        <ThemedText type="small" style={styles.muted}>
          Large diff — showing all of it may be slow.
        </ThemedText>
      ) : null}

      {file.hunks.length === 0 ? (
        <ThemedText type="small" style={styles.muted}>
          No textual changes (binary, mode change, or rename only).
        </ThemedText>
      ) : (
        file.hunks.map((hunk, i) => <Hunk key={`${i}-${hunk.old_start}`} hunk={hunk} />)
      )}
    </View>
  );
}

function Hunk({ hunk }: { hunk: DiffHunk }) {
  return (
    <View style={styles.hunk}>
      <ThemedText type="code" style={styles.hunkHeader} numberOfLines={1}>
        @@ -{hunk.old_start},{hunk.old_lines} +{hunk.new_start},{hunk.new_lines} @@
        {hunk.header_suffix ? ` ${hunk.header_suffix}` : ''}
      </ThemedText>
      <ScrollView horizontal showsHorizontalScrollIndicator={false}>
        <View>
          {hunk.lines.map((line, i) => (
            <Line key={i} line={line} />
          ))}
        </View>
      </ScrollView>
    </View>
  );
}

function Line({ line }: { line: DiffLine }) {
  if (line.kind === 'NoNewlineHint') {
    return (
      <ThemedText type="code" style={[styles.line, styles.muted]}>
        ⏎ No newline at end of file
      </ThemedText>
    );
  }
  const added = line.kind === 'Added';
  const removed = line.kind === 'Removed';
  return (
    <ThemedText
      type="code"
      style={[
        styles.line,
        added && { backgroundColor: ADDED_BG },
        removed && { backgroundColor: REMOVED_BG },
      ]}
    >
      {added ? '+' : removed ? '-' : ' '}
      {line.content}
    </ThemedText>
  );
}

const styles = StyleSheet.create({
  file: { gap: Spacing.two },
  path: { fontWeight: '700' },
  hunk: { gap: Spacing.half },
  hunkHeader: { opacity: 0.6 },
  line: { lineHeight: 16, paddingHorizontal: Spacing.one },
  muted: { opacity: 0.7 },
});
