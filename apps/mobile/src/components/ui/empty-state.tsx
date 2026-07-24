import { type LucideIcon } from 'lucide-react-native';
import { StyleSheet, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Button } from '@/components/ui/button';
import { Icon } from '@/components/ui/icon';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';

type Props = {
  title: string;
  /** Optional secondary line explaining *why* it is empty. */
  message?: string;
  icon?: LucideIcon;
  /** Optional recovery affordance, e.g. "Retry" when the host is unreachable. */
  action?: { label: string; onPress: () => void; loading?: boolean };
};

/**
 * The one empty-state layout: an optional muted icon, a title, an optional
 * explanatory line, and an optional action button — centred. Replaces the
 * per-screen ad-hoc muted text so every "nothing here" reads the same.
 */
export function EmptyState({ title, message, icon, action }: Props) {
  const theme = useTheme();
  return (
    <View style={styles.wrap}>
      {icon ? <Icon icon={icon} size="xl" color={theme.textMuted} /> : null}
      <ThemedText type="small" style={styles.title}>
        {title}
      </ThemedText>
      {message ? (
        <ThemedText type="small" style={[styles.message, { color: theme.textSecondary }]}>
          {message}
        </ThemedText>
      ) : null}
      {action ? (
        <Button
          label={action.label}
          variant="secondary"
          size="compact"
          loading={action.loading}
          onPress={action.onPress}
          style={styles.action}
        />
      ) : null}
    </View>
  );
}

const styles = StyleSheet.create({
  wrap: { alignItems: 'center', gap: Spacing.two, paddingTop: Spacing.five, paddingHorizontal: Spacing.four },
  title: { textAlign: 'center' },
  message: { textAlign: 'center' },
  action: { marginTop: Spacing.two },
});
