import { StyleSheet, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Button } from '@/components/ui/button';
import { Radius, Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';

type Props = {
  onBack: () => void;
};

/**
 * What stands where the composer stood once the desktop has closed the session.
 *
 * Deliberately not the danger-toned error banner: nothing went wrong and there
 * is nothing to retry — someone closed the tab at the desktop, which is a normal
 * thing to do. It occupies the composer's place so the input cannot be reached,
 * rather than leaving a live-looking box that fails on send.
 */
export function SessionClosedNotice({ onBack }: Props) {
  const theme = useTheme();
  return (
    <View style={[styles.wrap, { borderTopColor: theme.border, backgroundColor: theme.surface1 }]}>
      <ThemedText type="small" themeColor="textSecondary" style={styles.text}>
        This session was closed on the desktop.
      </ThemedText>
      <Button label="Back to sessions" variant="secondary" size="compact" onPress={onBack} />
    </View>
  );
}

const styles = StyleSheet.create({
  wrap: {
    borderTopWidth: StyleSheet.hairlineWidth,
    borderTopLeftRadius: Radius.md,
    borderTopRightRadius: Radius.md,
    paddingHorizontal: Spacing.four,
    paddingVertical: Spacing.three,
    gap: Spacing.two,
    alignItems: 'center',
  },
  text: { textAlign: 'center' },
});
