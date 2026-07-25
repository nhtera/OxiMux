import { type LucideIcon } from 'lucide-react-native';
import {
  ActivityIndicator,
  Pressable,
  type PressableProps,
  StyleSheet,
  type StyleProp,
  type ViewStyle,
} from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Icon } from '@/components/ui/icon';
import { ControlHeight, type ControlSize } from '@/components/ui/control-geometry';
import { Radius, Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';

export type ButtonVariant = 'primary' | 'secondary' | 'outline' | 'ghost' | 'danger';

type Props = Omit<PressableProps, 'style' | 'children'> & {
  label: string;
  variant?: ButtonVariant;
  size?: Extract<ControlSize, 'compact' | 'field'>;
  leftIcon?: LucideIcon;
  loading?: boolean;
  style?: StyleProp<ViewStyle>;
};

/**
 * The one button. Variants map to the token palette; a single implementation so
 * every screen's buttons share padding, radius, press feedback and disabled
 * treatment instead of each re-deriving them.
 */
export function Button({
  label,
  variant = 'secondary',
  size = 'field',
  leftIcon,
  loading = false,
  disabled,
  style,
  ...rest
}: Props) {
  const theme = useTheme();
  const isDisabled = disabled || loading;

  // Background / border / text resolved per variant from semantic tokens.
  const bg =
    variant === 'primary'
      ? theme.accent
      : variant === 'secondary'
        ? theme.surface2
        : 'transparent';
  const border =
    variant === 'outline'
      ? theme.borderStrong
      : variant === 'danger'
        ? theme.danger
        : 'transparent';
  const fg =
    variant === 'primary'
      ? theme.accentText
      : variant === 'danger'
        ? theme.danger
        : theme.text;

  return (
    <Pressable
      accessibilityRole="button"
      accessibilityState={{ disabled: !!isDisabled, busy: loading }}
      disabled={isDisabled}
      style={({ pressed }) => [
        styles.base,
        {
          minHeight: ControlHeight[size],
          borderRadius: Radius.md,
          backgroundColor: bg,
          borderColor: border,
          borderWidth: variant === 'outline' || variant === 'danger' ? 1 : 0,
        },
        pressed && styles.pressed,
        isDisabled && styles.disabled,
        style,
      ]}
      {...rest}
    >
      {/* Swap the leading icon for a spinner in place so loading never shifts
          the label. */}
      {loading ? (
        <ActivityIndicator size="small" color={fg} />
      ) : leftIcon ? (
        <Icon icon={leftIcon} size="sm" color={fg} />
      ) : null}
      <ThemedText type="smallBold" style={{ color: fg }}>
        {label}
      </ThemedText>
    </Pressable>
  );
}

// Exported so an icon-only sibling (IconButton) can share the exact geometry.
export const buttonStyles = StyleSheet.create({
  base: {
    flexDirection: 'row',
    alignItems: 'center',
    justifyContent: 'center',
    gap: Spacing.two,
    paddingHorizontal: Spacing.three,
  },
  pressed: { opacity: 0.85 },
  disabled: { opacity: 0.5 },
});

const styles = buttonStyles;
