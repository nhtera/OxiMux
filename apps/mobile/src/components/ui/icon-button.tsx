import { type LucideIcon } from 'lucide-react-native';
import { Pressable, type PressableProps, StyleSheet, type StyleProp, type ViewStyle } from 'react-native';

import { Icon } from '@/components/ui/icon';
import { ControlHeight, IconHitSlop } from '@/components/ui/control-geometry';
import { IconSize, Radius } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';

type Props = Omit<PressableProps, 'style' | 'children'> & {
  icon: LucideIcon;
  /** Required — an icon-only control has no visible text for assistive tech. */
  accessibilityLabel: string;
  size?: keyof typeof IconSize;
  color?: string;
  style?: StyleProp<ViewStyle>;
};

/**
 * Icon-only tappable. Carries its own hit-slop so a 16pt glyph still meets the
 * touch-target floor, and a pressed-opacity so every icon action gives feedback.
 */
export function IconButton({
  icon,
  accessibilityLabel,
  size = 'lg',
  color,
  disabled,
  style,
  ...rest
}: Props) {
  const theme = useTheme();
  return (
    <Pressable
      accessibilityRole="button"
      accessibilityLabel={accessibilityLabel}
      accessibilityState={{ disabled: !!disabled }}
      disabled={disabled}
      hitSlop={IconHitSlop}
      style={({ pressed }) => [styles.base, pressed && styles.pressed, disabled && styles.disabled, style]}
      {...rest}
    >
      <Icon icon={icon} size={size} color={color ?? theme.text} />
    </Pressable>
  );
}

const styles = StyleSheet.create({
  base: {
    minHeight: ControlHeight.compact,
    minWidth: ControlHeight.compact,
    alignItems: 'center',
    justifyContent: 'center',
    borderRadius: Radius.md,
  },
  pressed: { opacity: 0.6 },
  disabled: { opacity: 0.4 },
});
