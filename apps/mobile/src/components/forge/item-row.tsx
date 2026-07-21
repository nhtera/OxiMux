import { Pressable, StyleSheet, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { relativeAge } from '@/native/forge';
import type { ForgeItem } from 'oximux-core';

/** Colour for an item's lifecycle state, mirroring forge conventions. */
function stateColor(state: string): string {
  switch (state.toUpperCase()) {
    case 'MERGED':
      return '#A371F7';
    case 'CLOSED':
      return '#F85149';
    default:
      return '#3FB950';
  }
}

/**
 * One issue or PR.
 *
 * Mirrors the desktop Tasks table's information layout — number, title, then a
 * context sub-line — rather than inventing a phone-specific one, so someone
 * moving between the two reads the same row the same way.
 */
export function ForgeItemRow({
  item,
  onPress,
  onAttach,
}: {
  item: ForgeItem;
  onPress: () => void;
  onAttach: () => void;
}) {
  const theme = useTheme();
  const age = relativeAge(item.updatedAt);

  return (
    <View style={[styles.row, { borderBottomColor: theme.backgroundElement }]}>
      <Pressable style={styles.main} onPress={onPress}>
        <View style={styles.titleLine}>
          <View style={[styles.stateDot, { backgroundColor: stateColor(item.state) }]} />
          <ThemedText numberOfLines={2} style={styles.title}>
            {item.title}
          </ThemedText>
        </View>
        <ThemedText type="small" style={styles.meta} numberOfLines={1}>
          {/* Assembled from what is actually present: the forge omits an author
              for deleted accounts and a timestamp on older CLIs, and rendering
              "by  ·  ·" around the gaps looks broken. */}
          {[`#${item.number}`, item.author && `by ${item.author}`, age]
            .filter(Boolean)
            .join(' · ')}
        </ThemedText>
        {item.labels.length > 0 ? (
          <ThemedText type="small" style={styles.meta} numberOfLines={1}>
            {item.labels.join(', ')}
          </ThemedText>
        ) : null}
      </Pressable>
      <Pressable onPress={onAttach} hitSlop={Spacing.two} style={styles.attach}>
        <ThemedText type="code">Attach</ThemedText>
      </Pressable>
    </View>
  );
}

const styles = StyleSheet.create({
  row: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: Spacing.two,
    paddingVertical: Spacing.three,
    paddingHorizontal: Spacing.three,
    borderBottomWidth: StyleSheet.hairlineWidth,
  },
  main: { flex: 1, gap: Spacing.one },
  titleLine: { flexDirection: 'row', alignItems: 'flex-start', gap: Spacing.two },
  stateDot: { width: 8, height: 8, borderRadius: 4, marginTop: 6 },
  title: { flex: 1 },
  meta: { opacity: 0.7 },
  attach: { paddingVertical: Spacing.two, paddingHorizontal: Spacing.two },
});
