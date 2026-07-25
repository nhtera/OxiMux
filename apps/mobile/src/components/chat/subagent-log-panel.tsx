import { useState } from 'react';
import { Pressable, ScrollView, StyleSheet, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Spacing } from '@/constants/theme';

/**
 * What a subagent said while it worked, behind a tap.
 *
 * Collapsed by default because a subagent run can be long and it is supporting
 * detail, not the answer — expanded by default it would push the actual reply
 * off the screen.
 *
 * Rendered as plain monospace, **not** through the markdown renderer. These are
 * log lines, and CommonMark would mangle ones that happen to begin with `#` or
 * contain `_`, turning a heading out of a comment or italics out of a
 * snake_case identifier.
 *
 * The body scrolls inside a capped height rather than growing the transcript:
 * a 200-line log inside a list row would make one entry taller than several
 * screens and strand the scroll position.
 */
export function SubagentLogPanel({ lines }: { lines: string[] }) {
  const [expanded, setExpanded] = useState(false);
  if (lines.length === 0) return null;

  return (
    <View style={styles.wrap}>
      <Pressable onPress={() => setExpanded((v) => !v)} hitSlop={Spacing.two}>
        <ThemedText type="small" style={styles.toggle}>
          {expanded ? '▾' : '▸'} Subagent activity ({lines.length})
        </ThemedText>
      </Pressable>

      {expanded ? (
        <ScrollView style={styles.body} nestedScrollEnabled>
          {lines.map((line, i) => (
            <ThemedText key={i} type="code" style={styles.line}>
              {line}
            </ThemedText>
          ))}
        </ScrollView>
      ) : null}
    </View>
  );
}

const styles = StyleSheet.create({
  wrap: { gap: Spacing.one },
  toggle: { opacity: 0.7 },
  // Capped so a long log scrolls within itself instead of stretching the row.
  body: { maxHeight: 220 },
  line: { opacity: 0.85, lineHeight: 16 },
});
