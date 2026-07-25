import { Check } from 'lucide-react-native';
import type { Choice } from 'oximux-core';
import { StyleSheet } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { ListRow } from '@/components/ui/list-row';
import { Sheet } from '@/components/ui/sheet';
import { Radius, Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';

/**
 * A sheet for picking a model or a permission mode.
 *
 * One component for both because the two are the same shape on the wire — a
 * list of `Choice` plus which one is current — and giving them separate
 * components would be duplication that drifts.
 *
 * The current entry is marked rather than merely pre-selected: switching costs a
 * round trip and can be refused outright by a fix-at-spawn backend, so knowing
 * what is already running is what stops a pointless tap.
 */
export function ChoicePicker({
  title,
  choices,
  current,
  visible,
  busy,
  onPick,
  onClose,
}: {
  title: string;
  choices: Choice[];
  current?: string;
  visible: boolean;
  busy: boolean;
  onPick: (id: string) => void;
  onClose: () => void;
}) {
  const theme = useTheme();

  return (
    <Sheet visible={visible} onClose={onClose}>
      <ThemedText type="smallBold" style={styles.title}>
        {title}
      </ThemedText>

      {choices.map((choice) => {
        const selected = choice.id === current;
        return (
          <ListRow
            key={choice.id}
            // Switching costs a round trip and can be refused outright, so the row
            // already showing what is current, or one tapped while a switch is in
            // flight, takes no press at all.
            onPress={selected || busy ? undefined : () => onPick(choice.id)}
            accessoryIcon={selected ? Check : undefined}
            style={[
              styles.row,
              selected && { backgroundColor: theme.backgroundSelected },
              busy && styles.busy,
            ]}
          >
            <ThemedText numberOfLines={1}>{choice.label}</ThemedText>
            {choice.description ? (
              <ThemedText type="small" numberOfLines={2} style={styles.muted}>
                {choice.description}
              </ThemedText>
            ) : null}
          </ListRow>
        );
      })}
    </Sheet>
  );
}

const styles = StyleSheet.create({
  title: { paddingBottom: Spacing.one },
  row: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: Spacing.three,
    paddingVertical: Spacing.three,
    paddingHorizontal: Spacing.two,
    borderRadius: Radius.md,
  },
  muted: { opacity: 0.7 },
  busy: { opacity: 0.5 },
});
