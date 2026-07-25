import { ChevronDown, ChevronRight, Folder, FolderOpen, SquarePen } from 'lucide-react-native';
import { Pressable, StyleSheet, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Icon } from '@/components/ui/icon';
import { IconSize, Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { tick } from '@/native/haptics';

/**
 * How far a session row is indented so its title lines up with the project name
 * above it. Exported because the row that does the indenting lives in its own
 * file, and the two drifting apart is exactly the misalignment this replaces.
 */
export const PROJECT_INDENT = IconSize.lg + Spacing.two;

type Props = {
  name: string;
  /** How many sessions the project holds — shown while collapsed, since the rows
   *  that would otherwise answer are hidden. */
  count: number;
  expanded: boolean;
  onToggle: () => void;
  /** Absent for the synthetic bucket of sessions matching no project: there is no
   *  path to start one in, so it gets no compose affordance. */
  onCompose?: () => void;
};

/**
 * A project's row in the session list: a folder, its name, and a chevron that
 * expands or collapses the sessions beneath it.
 *
 * Replaces an uppercase, letter-spaced caption that only labelled a run of rows.
 * A project with a dozen sessions could not be folded away, and one with none
 * rendered as a bare line of text with nothing under it — which read as a
 * glitch rather than as an empty project. Naming it like a folder makes both
 * states legible: closed is a normal state, not a missing one.
 *
 * The whole row is the toggle rather than the chevron alone — a 14pt glyph is
 * not a comfortable target, and the name is what a thumb aims at. Compose nests
 * inside it: a press on the inner control is handled there and does not reach
 * the row, so composing never also folds the project it is adding to.
 *
 * No press styling of its own. The row dimming under the finger read as a blink
 * — white text greying and coming back — and it is redundant here: the folder
 * opens, the chevron turns and the rows move, all on the same touch. A tap that
 * changes what you are looking at does not also need to flash.
 */
export function ProjectGroupHeader({ name, count, expanded, onToggle, onCompose }: Props) {
  const theme = useTheme();
  return (
    <Pressable
      onPress={() => {
        tick();
        onToggle();
      }}
      accessibilityRole="button"
      accessibilityState={{ expanded }}
      accessibilityLabel={`${name}, ${count} ${count === 1 ? 'session' : 'sessions'}`}
      style={styles.header}
    >
      <View style={styles.title}>
        {/* Open and shut are drawn, not just implied by the chevron: the folder
            is the larger, higher-contrast glyph, so it is what the eye reads
            first when scanning which projects are unfolded. */}
        <Icon icon={expanded ? FolderOpen : Folder} size="lg" color={theme.textSecondary} />
        <ThemedText numberOfLines={1} style={styles.name}>
          {name}
        </ThemedText>
        <Icon icon={expanded ? ChevronDown : ChevronRight} size="sm" color={theme.textMuted} />
        {/* Only while collapsed: with the rows on screen the number is just
            arithmetic the user can already see. */}
        {!expanded && count > 0 ? (
          <ThemedText type="small" themeColor="textMuted">
            {count}
          </ThemedText>
        ) : null}
      </View>
      {onCompose ? (
        <Pressable
          onPress={() => {
            tick();
            onCompose();
          }}
          accessibilityRole="button"
          accessibilityLabel={`New session in ${name}`}
          hitSlop={Spacing.three}
          style={({ pressed }) => pressed && { opacity: 0.5 }}
        >
          <Icon icon={SquarePen} size="lg" color={theme.textSecondary} />
        </Pressable>
      ) : null}
    </Pressable>
  );
}

const styles = StyleSheet.create({
  header: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: Spacing.three,
    paddingHorizontal: Spacing.four,
    paddingTop: Spacing.three,
    paddingBottom: Spacing.two,
  },
  // `flex: 1` so the name cluster takes the space between the folder and the
  // compose icon, leaving the whole width pressable.
  title: { flex: 1, flexDirection: 'row', alignItems: 'center', gap: Spacing.two },
  // `flexShrink` rather than `flex: 1`: a short project name should leave the
  // chevron beside it, the way a disclosure control reads, not pushed to the far
  // side of the row.
  name: { flexShrink: 1 },
});
