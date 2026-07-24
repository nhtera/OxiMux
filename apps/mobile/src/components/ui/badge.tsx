import { Pressable, StyleSheet, type StyleProp, View, type ViewStyle } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Radius, Spacing } from '@/constants/theme';
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

/** `#rrggbb` → `rgba(r,g,b,a)` so a tone colour can tint at low alpha. */
function withAlpha(hex: string, alpha: number): string {
  const h = hex.replace('#', '');
  const r = parseInt(h.slice(0, 2), 16);
  const g = parseInt(h.slice(2, 4), 16);
  const b = parseInt(h.slice(4, 6), 16);
  return `rgba(${r}, ${g}, ${b}, ${alpha})`;
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
 */
export function Chip({
  label,
  selected = false,
  busy = false,
  onPress,
  style,
}: {
  label: string;
  selected?: boolean;
  busy?: boolean;
  onPress?: () => void;
  style?: StyleProp<ViewStyle>;
}) {
  const theme = useTheme();
  return (
    <Pressable
      onPress={onPress}
      disabled={busy || !onPress}
      accessibilityRole="button"
      accessibilityState={{ selected, disabled: busy }}
      style={({ pressed }) => [
        styles.chip,
        {
          borderColor: selected ? theme.accent : theme.borderStrong,
          backgroundColor: selected ? theme.accent : 'transparent',
        },
        pressed && styles.pressed,
        busy && styles.busy,
        style,
      ]}
    >
      <ThemedText type="small" numberOfLines={1} style={selected ? { color: theme.accentText } : undefined}>
        {label}
      </ThemedText>
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
    borderWidth: StyleSheet.hairlineWidth,
    borderRadius: Radius.full,
    paddingVertical: Spacing.half,
    paddingHorizontal: Spacing.two,
    maxWidth: 180,
  },
  pressed: { opacity: 0.7 },
  busy: { opacity: 0.5 },
});
