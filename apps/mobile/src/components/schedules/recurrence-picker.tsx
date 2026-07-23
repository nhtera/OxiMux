import { Recurrence, Recurrence_Tags } from 'oximux-core';
import { Pressable, StyleSheet, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { INTERVAL_CHOICES, WEEKDAYS, intervalLabel } from '@/native/recurrence';

/**
 * Edits a {@link Recurrence} with pickers instead of a cron string.
 *
 * The desktop models recurrence as a closed set of three cases precisely so a
 * phone can drive it with controls that cannot produce an invalid value — no
 * `0 9 * * 1-5` to mistype, no client-side parser to keep in sync. This mirrors
 * that: a segmented control chooses the case, and each case exposes only values
 * inside the desktop's own floors (interval ≥ 5 minutes, a real time of day).
 *
 * Switching case does not try to carry a partial value across shapes — it snaps
 * to that case's sensible default. Preserving "every 30 minutes" as it becomes a
 * weekly time would be guessing at an intent the user has just changed.
 */
export function RecurrencePicker({
  value,
  onChange,
}: {
  value: Recurrence;
  onChange: (r: Recurrence) => void;
}) {
  return (
    <View style={styles.container}>
      <Segmented
        options={[
          ['Interval', Recurrence_Tags.EveryMinutes],
          ['Daily', Recurrence_Tags.DailyAt],
          ['Weekly', Recurrence_Tags.WeeklyAt],
        ]}
        selected={value.tag}
        onSelect={(tag) => onChange(defaultFor(tag, value))}
      />

      {value.tag === Recurrence_Tags.EveryMinutes ? (
        <IntervalChoice
          minutes={value.inner.minutes}
          onChange={(minutes) => onChange(new Recurrence.EveryMinutes({ minutes }))}
        />
      ) : null}

      {value.tag === Recurrence_Tags.DailyAt ? (
        <TimeOfDay
          hour={value.inner.hour}
          minute={value.inner.minute}
          onChange={(hour, minute) => onChange(new Recurrence.DailyAt({ hour, minute }))}
        />
      ) : null}

      {value.tag === Recurrence_Tags.WeeklyAt ? (
        <View style={styles.stack}>
          <WeekdayChoice
            weekday={value.inner.weekday}
            onChange={(weekday) =>
              onChange(
                new Recurrence.WeeklyAt({
                  weekday,
                  hour: value.inner.hour,
                  minute: value.inner.minute,
                }),
              )
            }
          />
          <TimeOfDay
            hour={value.inner.hour}
            minute={value.inner.minute}
            onChange={(hour, minute) =>
              onChange(new Recurrence.WeeklyAt({ weekday: value.inner.weekday, hour, minute }))
            }
          />
        </View>
      ) : null}
    </View>
  );
}

/** The default recurrence for a freshly created schedule: daily at 09:00. */
export function defaultRecurrence(): Recurrence {
  return new Recurrence.DailyAt({ hour: 9, minute: 0 });
}

/**
 * A sensible member of `tag`'s case. Carries the current time of day when both
 * the old and new case have one (daily ↔ weekly), so switching between the two
 * wall-clock modes does not silently reset a time the user already chose.
 */
function defaultFor(tag: Recurrence_Tags, current: Recurrence): Recurrence {
  const time =
    current.tag === Recurrence_Tags.DailyAt || current.tag === Recurrence_Tags.WeeklyAt
      ? { hour: current.inner.hour, minute: current.inner.minute }
      : { hour: 9, minute: 0 };
  switch (tag) {
    case Recurrence_Tags.EveryMinutes:
      return new Recurrence.EveryMinutes({ minutes: 30 });
    case Recurrence_Tags.DailyAt:
      return new Recurrence.DailyAt(time);
    case Recurrence_Tags.WeeklyAt:
      return new Recurrence.WeeklyAt({ weekday: 0, ...time });
  }
}

function IntervalChoice({
  minutes,
  onChange,
}: {
  minutes: number;
  onChange: (minutes: number) => void;
}) {
  return (
    <View style={styles.chips}>
      {INTERVAL_CHOICES.map((m) => (
        <Chip key={m} label={intervalLabel(m)} selected={m === minutes} onPress={() => onChange(m)} />
      ))}
    </View>
  );
}

function WeekdayChoice({
  weekday,
  onChange,
}: {
  weekday: number;
  onChange: (weekday: number) => void;
}) {
  return (
    <View style={styles.chips}>
      {WEEKDAYS.map((label, index) => (
        <Chip key={label} label={label} selected={index === weekday} onPress={() => onChange(index)} />
      ))}
    </View>
  );
}

/**
 * Hour and minute steppers. Both wrap, so a user can reach 23:00 or 00:55
 * without a long press. Minutes step by five — the store accepts any minute, but
 * a per-minute stepper on a phone is a lot of taps for a precision a scheduled
 * agent run does not need.
 */
function TimeOfDay({
  hour,
  minute,
  onChange,
}: {
  hour: number;
  minute: number;
  onChange: (hour: number, minute: number) => void;
}) {
  return (
    <View style={styles.time}>
      <Stepper
        label="Hour"
        value={pad(hour)}
        onDecrement={() => onChange((hour + 23) % 24, minute)}
        onIncrement={() => onChange((hour + 1) % 24, minute)}
      />
      <ThemedText type="title" style={styles.colon}>
        :
      </ThemedText>
      <Stepper
        label="Minute"
        value={pad(minute)}
        onDecrement={() => onChange(hour, (minute + 55) % 60)}
        onIncrement={() => onChange(hour, (minute + 5) % 60)}
      />
    </View>
  );
}

function pad(n: number): string {
  return String(n).padStart(2, '0');
}

function Segmented<T extends string | number>({
  options,
  selected,
  onSelect,
}: {
  options: [string, T][];
  selected: T;
  onSelect: (value: T) => void;
}) {
  const theme = useTheme();
  return (
    <View style={[styles.segmented, { backgroundColor: theme.backgroundElement }]}>
      {options.map(([label, value]) => (
        <Pressable
          key={label}
          onPress={() => onSelect(value)}
          style={[
            styles.segment,
            selected === value && { backgroundColor: theme.backgroundSelected },
          ]}
        >
          <ThemedText type="code">{label}</ThemedText>
        </Pressable>
      ))}
    </View>
  );
}

function Chip({
  label,
  selected,
  onPress,
}: {
  label: string;
  selected: boolean;
  onPress: () => void;
}) {
  const theme = useTheme();
  return (
    <Pressable
      onPress={onPress}
      style={[
        styles.chip,
        { backgroundColor: selected ? theme.backgroundSelected : theme.backgroundElement },
      ]}
    >
      <ThemedText type="code">{label}</ThemedText>
    </Pressable>
  );
}

function Stepper({
  label,
  value,
  onDecrement,
  onIncrement,
}: {
  label: string;
  value: string;
  onDecrement: () => void;
  onIncrement: () => void;
}) {
  const theme = useTheme();
  return (
    <View style={styles.stepper}>
      <ThemedText type="small" style={styles.stepperLabel}>
        {label}
      </ThemedText>
      <View style={styles.stepperControls}>
        <Pressable
          onPress={onDecrement}
          style={[styles.stepperButton, { backgroundColor: theme.backgroundElement }]}
          accessibilityLabel={`Decrease ${label.toLowerCase()}`}
        >
          <ThemedText type="title">−</ThemedText>
        </Pressable>
        <ThemedText type="title" style={styles.stepperValue}>
          {value}
        </ThemedText>
        <Pressable
          onPress={onIncrement}
          style={[styles.stepperButton, { backgroundColor: theme.backgroundElement }]}
          accessibilityLabel={`Increase ${label.toLowerCase()}`}
        >
          <ThemedText type="title">+</ThemedText>
        </Pressable>
      </View>
    </View>
  );
}

const styles = StyleSheet.create({
  container: { gap: Spacing.three },
  stack: { gap: Spacing.three },
  segmented: {
    flexDirection: 'row',
    borderRadius: Spacing.two,
    padding: Spacing.half,
    gap: Spacing.half,
  },
  segment: {
    flex: 1,
    alignItems: 'center',
    paddingVertical: Spacing.two,
    borderRadius: Spacing.two,
  },
  chips: { flexDirection: 'row', flexWrap: 'wrap', gap: Spacing.two },
  chip: {
    paddingVertical: Spacing.two,
    paddingHorizontal: Spacing.three,
    borderRadius: Spacing.two,
  },
  time: { flexDirection: 'row', alignItems: 'flex-end', gap: Spacing.three },
  colon: { paddingBottom: Spacing.two },
  stepper: { gap: Spacing.one },
  stepperLabel: { opacity: 0.7 },
  stepperControls: { flexDirection: 'row', alignItems: 'center', gap: Spacing.three },
  stepperButton: {
    width: 44,
    height: 44,
    alignItems: 'center',
    justifyContent: 'center',
    borderRadius: Spacing.two,
  },
  stepperValue: { minWidth: 36, textAlign: 'center' },
});
