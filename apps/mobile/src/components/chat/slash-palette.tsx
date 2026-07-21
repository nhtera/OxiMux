import { Pressable, ScrollView, StyleSheet, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';

/**
 * The slash-command query the composer's text implies, or `undefined` when the
 * palette should stay closed.
 *
 * Only a `/` that begins the message opens the palette. A slash mid-sentence is
 * a path, a fraction, or a date far more often than it is a command, and the
 * agent takes a command as ordinary text anyway — so opening on every `/` would
 * interrupt normal typing to offer something that is usually wrong.
 *
 * Returns the text after the slash so the caller can filter; an empty string
 * means a bare `/`, which lists everything.
 */
export function slashQuery(text: string): string | undefined {
  if (!text.startsWith('/')) return undefined;
  const rest = text.slice(1);
  // A space ends the command name — by then the user is writing arguments and
  // the palette would be covering the input for no reason.
  if (/\s/.test(rest)) return undefined;
  return rest;
}

/** Commands matching `query`, case-insensitively. */
export function filterCommands(commands: string[], query: string): string[] {
  if (query.length === 0) return commands;
  const needle = query.toLowerCase();
  return commands.filter((c) => c.toLowerCase().includes(needle));
}

/**
 * A list of the agent's slash commands, shown above the composer while the
 * message is a bare command name.
 *
 * Names only: the wire carries `slash_commands` as a list of strings with no
 * descriptions (`ThreadJson.slash_commands`), so there is nothing else to show
 * yet. Descriptions exist on the desktop's `ChatThread` but are not projected
 * into the snapshot — adding them is a follow-up, not a blocker.
 */
export function SlashPalette({
  commands,
  onSelect,
}: {
  commands: string[];
  onSelect: (command: string) => void;
}) {
  const theme = useTheme();
  // An older host, or an agent with none configured. Rendering an empty dropdown
  // would look broken; showing nothing is correct.
  if (commands.length === 0) return null;

  return (
    <View style={[styles.palette, { backgroundColor: theme.backgroundElement }]}>
      <ScrollView
        // The composer's input keeps focus while the list is tapped: dismissing
        // the keyboard would collapse the layout under the user's finger between
        // press-in and press-out, so the tap would land somewhere else.
        keyboardShouldPersistTaps="always"
        nestedScrollEnabled
        style={styles.scroll}
      >
        {commands.map((command) => (
          <Pressable
            key={command}
            onPress={() => onSelect(command)}
            style={styles.row}
            accessibilityRole="button"
          >
            <ThemedText type="code">/{command}</ThemedText>
          </Pressable>
        ))}
      </ScrollView>
    </View>
  );
}

const styles = StyleSheet.create({
  palette: { borderRadius: Spacing.two, overflow: 'hidden' },
  // Capped so a long command list cannot push the input off the screen.
  scroll: { maxHeight: 180 },
  row: { paddingVertical: Spacing.two, paddingHorizontal: Spacing.three },
});
