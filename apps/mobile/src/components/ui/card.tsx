import { StyleSheet, type StyleProp, View, type ViewProps, type ViewStyle } from 'react-native';

import { Radius, Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';

type Props = ViewProps & {
  /** Elevation step. `1` = subtle raise (default), `2` = more prominent. */
  level?: 1 | 2;
  bordered?: boolean;
  padded?: boolean;
  style?: StyleProp<ViewStyle>;
};

/** A raised surface. Elevation is a token step, not a bespoke colour per screen. */
export function Card({ level = 1, bordered = true, padded = true, style, ...rest }: Props) {
  const theme = useTheme();
  return (
    <View
      style={[
        styles.base,
        padded && styles.padded,
        {
          backgroundColor: level === 2 ? theme.surface2 : theme.surface1,
          borderColor: theme.border,
          borderWidth: bordered ? StyleSheet.hairlineWidth : 0,
        },
        style,
      ]}
      {...rest}
    />
  );
}

const styles = StyleSheet.create({
  base: { borderRadius: Radius.lg },
  padded: { padding: Spacing.three, gap: Spacing.two },
});
