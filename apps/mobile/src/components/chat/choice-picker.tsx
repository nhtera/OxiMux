import type { Choice } from 'oximux-core';
import { Modal, Pressable, ScrollView, StyleSheet, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Spacing } from '@/constants/theme';
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
    <Modal visible={visible} transparent animationType="slide" onRequestClose={onClose}>
      {/* Tapping the backdrop dismisses — the sheet is a choice, not a decision
          the user must make before doing anything else. */}
      <Pressable style={styles.backdrop} onPress={onClose}>
        {/* Swallows the press so a tap inside the sheet does not close it. */}
        <Pressable
          style={[styles.sheet, { backgroundColor: theme.background }]}
          onPress={() => {}}
        >
          <ThemedText type="smallBold" style={styles.title}>
            {title}
          </ThemedText>

          <ScrollView style={styles.list}>
            {choices.map((choice) => {
              const selected = choice.id === current;
              return (
                <Pressable
                  key={choice.id}
                  disabled={busy || selected}
                  onPress={() => onPick(choice.id)}
                  accessibilityRole="radio"
                  accessibilityState={{ selected, disabled: busy }}
                  style={[
                    styles.row,
                    selected && { backgroundColor: theme.backgroundSelected },
                    busy && styles.busy,
                  ]}
                >
                  <View style={styles.rowText}>
                    <ThemedText numberOfLines={1}>{choice.label}</ThemedText>
                    {choice.description ? (
                      <ThemedText type="small" numberOfLines={2} style={styles.muted}>
                        {choice.description}
                      </ThemedText>
                    ) : null}
                  </View>
                  {selected ? <ThemedText type="code">✓</ThemedText> : null}
                </Pressable>
              );
            })}
          </ScrollView>
        </Pressable>
      </Pressable>
    </Modal>
  );
}

const styles = StyleSheet.create({
  backdrop: { flex: 1, justifyContent: 'flex-end', backgroundColor: 'rgba(0,0,0,0.4)' },
  sheet: {
    maxHeight: '70%',
    borderTopLeftRadius: Spacing.three,
    borderTopRightRadius: Spacing.three,
    padding: Spacing.four,
    gap: Spacing.two,
  },
  title: { paddingBottom: Spacing.one },
  list: { flexGrow: 0 },
  row: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: Spacing.three,
    paddingVertical: Spacing.three,
    paddingHorizontal: Spacing.two,
    borderRadius: Spacing.two,
  },
  rowText: { flex: 1, gap: Spacing.half },
  muted: { opacity: 0.7 },
  busy: { opacity: 0.5 },
});
