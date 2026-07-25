import { Pressable, StyleSheet } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Radius, Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';

type Props = {
  message: string;
  /** When set, the whole banner is tappable to clear the error. */
  onDismiss?: () => void;
};

/**
 * A dismissible error line: a calm danger-toned message on a faint danger
 * surface, tap-to-clear. Replaces the copy-pasted red error text each screen
 * defined in its own StyleSheet.
 */
export function ErrorBanner({ message, onDismiss }: Props) {
  const theme = useTheme();
  return (
    <Pressable
      onPress={onDismiss}
      disabled={!onDismiss}
      accessibilityRole={onDismiss ? 'button' : undefined}
      accessibilityLabel={onDismiss ? 'Dismiss error' : undefined}
      style={[styles.banner, { borderColor: theme.danger }]}
    >
      <ThemedText type="small" style={{ color: theme.danger }}>
        {message}
      </ThemedText>
    </Pressable>
  );
}

const styles = StyleSheet.create({
  banner: {
    borderWidth: StyleSheet.hairlineWidth,
    borderRadius: Radius.md,
    paddingHorizontal: Spacing.three,
    paddingVertical: Spacing.two,
  },
});
