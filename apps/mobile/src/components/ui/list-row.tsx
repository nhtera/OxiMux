import { type LucideIcon } from 'lucide-react-native';
import { Pressable, StyleSheet, type StyleProp, View, type ViewStyle } from 'react-native';

import { Icon } from '@/components/ui/icon';
import { IconSize, Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';

/**
 * A hairline rule between rows. Insetting it (default: a row's horizontal padding)
 * makes a scrolling list read as one grouped surface — the reference's mobile
 * lists lean on dividers, not whitespace, to separate rows.
 */
export function ListDivider({ inset = Spacing.four }: { inset?: number }) {
  const theme = useTheme();
  return <View style={{ height: StyleSheet.hairlineWidth, marginLeft: inset, backgroundColor: theme.border }} />;
}

type Props = {
  children: React.ReactNode;
  onPress?: () => void;
  /** Trailing chevron / status glyph. */
  accessoryIcon?: LucideIcon;
  accessibilityLabel?: string;
  style?: StyleProp<ViewStyle>;
};

/**
 * A tappable list row with consistent padding, gap and press feedback. Replaces
 * the per-screen `Pressable` rows that each set their own layout.
 */
export function ListRow({ children, onPress, accessoryIcon, accessibilityLabel, style }: Props) {
  const theme = useTheme();
  return (
    <Pressable
      onPress={onPress}
      disabled={!onPress}
      accessibilityRole={onPress ? 'button' : undefined}
      accessibilityLabel={accessibilityLabel}
      style={({ pressed }) => [
        styles.row,
        pressed && onPress ? { backgroundColor: theme.surface2 } : null,
        style,
      ]}
    >
      <View style={styles.content}>{children}</View>
      {accessoryIcon ? <Icon icon={accessoryIcon} size="sm" color={theme.textMuted} /> : null}
    </Pressable>
  );
}

const styles = StyleSheet.create({
  row: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: Spacing.three,
    paddingVertical: Spacing.two,
    minHeight: IconSize.xl + Spacing.three,
  },
  content: { flex: 1, gap: Spacing.half },
});
