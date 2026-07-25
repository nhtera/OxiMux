import type { Schedule } from 'oximux-core';
import { Pressable, StyleSheet, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Switch } from '@/components/ui/switch';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';

/**
 * One schedule: its name, the desktop's own phrasing of when it repeats, and the
 * next time it will fire. The recurrence line is `schedule.summary` verbatim —
 * the desktop derives it once and both surfaces read the same words, so the
 * phone never re-implements the "Weekdays at 09:00" phrasing.
 *
 * Tapping the row opens its run history; the switch toggles it without leaving
 * the list. A disabled schedule dims rather than hides — it is still a thing the
 * user made and may re-enable.
 */
export function ScheduleRow({
  schedule,
  onOpen,
  onToggle,
  onDelete,
}: {
  schedule: Schedule;
  onOpen: () => void;
  onToggle: (enabled: boolean) => void;
  /** Long-press to delete — confirmed by the caller, since it is irreversible. */
  onDelete: () => void;
}) {
  const theme = useTheme();
  return (
    <Pressable
      onPress={onOpen}
      onLongPress={onDelete}
      style={({ pressed }) => [styles.row, pressed && { backgroundColor: theme.surface2 }]}
    >
      <View style={[styles.text, !schedule.enabled && styles.dim]}>
        <ThemedText numberOfLines={1}>{schedule.name}</ThemedText>
        <ThemedText type="small" themeColor="textMuted" numberOfLines={1}>
          {schedule.summary}
        </ThemedText>
        <ThemedText type="small" themeColor="textMuted" numberOfLines={1}>
          {schedule.enabled ? `Next ${formatNextFire(schedule.nextFireAt)}` : 'Paused'}
        </ThemedText>
      </View>
      <Switch
        value={schedule.enabled}
        onValueChange={onToggle}
        accessibilityLabel={`${schedule.enabled ? 'Disable' : 'Enable'} ${schedule.name}`}
      />
    </Pressable>
  );
}

/**
 * The next fire in the reader's own locale and zone. The wire carries an RFC-3339
 * instant in the desktop's zone; `Date` re-expresses it here, so a phone in a
 * different timezone sees the fire at its own wall-clock, which is what a person
 * glancing at "next run" expects.
 */
function formatNextFire(rfc3339: string): string {
  const when = new Date(rfc3339);
  if (Number.isNaN(when.getTime())) return rfc3339;
  return when.toLocaleString(undefined, {
    weekday: 'short',
    hour: '2-digit',
    minute: '2-digit',
    month: 'short',
    day: 'numeric',
  });
}

const styles = StyleSheet.create({
  row: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: Spacing.three,
    paddingVertical: Spacing.three,
    paddingHorizontal: Spacing.four,
  },
  text: { flex: 1, gap: Spacing.half },
  dim: { opacity: 0.5 },
});
