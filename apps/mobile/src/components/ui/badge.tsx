import { type LucideIcon } from 'lucide-react-native';
import { Pressable, StyleSheet, type StyleProp, View, type ViewStyle } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { ControlHeight } from '@/components/ui/control-geometry';
import { Icon } from '@/components/ui/icon';
import { Radius, Spacing, withAlpha } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';

export type BadgeTone = 'neutral' | 'success' | 'danger' | 'warning' | 'info' | 'merged';

function toneColor(tone: BadgeTone, theme: ReturnType<typeof useTheme>): string {
  switch (tone) {
    case 'success':
      return theme.success;
    case 'danger':
      return theme.danger;
    case 'warning':
      return theme.warning;
    case 'info':
      return theme.info;
    case 'merged':
      return theme.merged;
    case 'neutral':
      return theme.textSecondary;
  }
}

/**
 * A static status pill: a tone-tinted fill + matching dot + coloured label, so
 * severity reads from the whole pill at a glance, not just a 6px dot. Neutral has
 * no colour to tint, so it keeps the plain surface background.
 */
export function Badge({ label, tone = 'neutral' }: { label: string; tone?: BadgeTone }) {
  const theme = useTheme();
  const color = toneColor(tone, theme);
  const tinted = tone !== 'neutral';
  return (
    <View
      style={[
        styles.badge,
        {
          borderColor: tinted ? withAlpha(color, 0.24) : theme.border,
          backgroundColor: tinted ? withAlpha(color, 0.14) : theme.surface2,
        },
      ]}
    >
      <View style={[styles.dot, { backgroundColor: color }]} />
      <ThemedText type="small" style={{ color }}>
        {label}
      </ThemedText>
    </View>
  );
}

/**
 * A selectable pill (model/mode picker, recurrence). One implementation for what
 * used to be two divergent `Chip`s. `selected` fills with the accent; `busy`
 * dims and blocks the press.
 *
 * `icon` and `trailing` let a chip say what kind of thing it changes without
 * spending label width on it — the composer's chips sit in a row with the send
 * button and have very little room, so "Sonnet ⌄" beats "Model: Sonnet".
 */
export function Chip({
  label,
  selected = false,
  busy = false,
  icon,
  trailing,
  onPress,
  style,
}: {
  label: string;
  selected?: boolean;
  busy?: boolean;
  /** Leading glyph — the *category* (model, permission mode). */
  icon?: LucideIcon;
  /** Trailing glyph — usually a chevron, marking the chip as a picker. */
  trailing?: LucideIcon;
  onPress?: () => void;
  style?: StyleProp<ViewStyle>;
}) {
  const theme = useTheme();
  // A filled chip needs its glyphs on the accent's text colour; an outlined one
  // keeps them quieter than the label so the label stays the thing being read.
  const glyph = selected ? theme.accentText : theme.textSecondary;
  return (
    <Pressable
      onPress={onPress}
      disabled={busy || !onPress}
      accessibilityRole="button"
      accessibilityState={{ selected, disabled: busy }}
      style={({ pressed }) => [
        styles.chip,
        {
          borderColor: selected ? theme.accent : theme.border,
          backgroundColor: selected ? theme.accent : theme.surface2,
        },
        pressed && styles.pressed,
        busy && styles.busy,
        style,
      ]}
    >
      {icon ? <Icon icon={icon} size="sm" color={glyph} /> : null}
      <ThemedText
        type="small"
        numberOfLines={1}
        // Shrinks before the chrome does, so a long model name truncates rather
        // than pushing the chevron (or the send button) off the row.
        style={[styles.chipLabel, selected ? { color: theme.accentText } : undefined]}
      >
        {label}
      </ThemedText>
      {trailing ? <Icon icon={trailing} size="sm" color={glyph} /> : null}
    </Pressable>
  );
}

const styles = StyleSheet.create({
  badge: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: Spacing.one,
    borderWidth: StyleSheet.hairlineWidth,
    borderRadius: Radius.full,
    paddingVertical: Spacing.half,
    paddingHorizontal: Spacing.two,
    alignSelf: 'flex-start',
  },
  dot: { width: 6, height: 6, borderRadius: 3 },
  chip: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: Spacing.one,
    minHeight: ControlHeight.tight,
    borderWidth: StyleSheet.hairlineWidth,
    borderRadius: Radius.full,
    paddingVertical: Spacing.half,
    paddingHorizontal: Spacing.two,
    maxWidth: 180,
    // Lets a row of chips give up width to a neighbour instead of overflowing.
    flexShrink: 1,
  },
  chipLabel: { flexShrink: 1 },
  pressed: { opacity: 0.7 },
  busy: { opacity: 0.5 },
});
