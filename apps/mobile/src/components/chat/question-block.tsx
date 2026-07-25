import { Check } from 'lucide-react-native';
import { Pressable, StyleSheet, TextInput, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Icon } from '@/components/ui/icon';
import { QUESTION_ACCENT } from '@/constants/chat';
import { Radius, Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import type { AskQuestion, QuestionAnswer } from '@/native/thread';

/**
 * One question and its option rows, driven entirely by its parent's answer draft.
 *
 * Single- and multi-select differ only in how a tap folds into `selected`, so
 * they share this component rather than forking: the wire shape is the same
 * `string[]` of option labels either way.
 */
export function QuestionBlock({
  question,
  answer,
  disabled,
  onChange,
}: {
  question: AskQuestion;
  answer: QuestionAnswer | undefined;
  disabled: boolean;
  onChange: (patch: Partial<QuestionAnswer>) => void;
}) {
  const theme = useTheme();
  const multi = question.kind === 'MultiSelect';
  const selected = answer?.selected ?? [];

  // Picking an option clears any custom text and vice versa: the two are
  // mutually exclusive on the Rust side (custom wins), so letting both look
  // active would misrepresent what actually gets sent.
  const toggle = (label: string) => {
    if (multi) {
      const next = selected.includes(label)
        ? selected.filter((l) => l !== label)
        : [...selected, label];
      onChange({ selected: next, custom: null });
    } else {
      onChange({ selected: selected[0] === label ? [] : [label], custom: null });
    }
  };

  return (
    <View style={styles.block}>
      {question.header ? (
        <ThemedText type="small" style={styles.header}>
          {question.header}
        </ThemedText>
      ) : null}
      <ThemedText style={styles.body}>{question.question}</ThemedText>

      {question.options.map((option) => {
        const on = selected.includes(option.label);
        return (
          <Pressable
            key={option.label}
            disabled={disabled}
            onPress={() => toggle(option.label)}
            style={[
              styles.option,
              {
                borderColor: on ? QUESTION_ACCENT : theme.backgroundSelected,
                backgroundColor: on ? theme.surface2 : 'transparent',
              },
              disabled && styles.dim,
            ]}
          >
            {/* A drawn control, not a font glyph: a checkbox for multi-select, a
                radio for single, filled with the accent when chosen — renders the
                same on every platform where ☑/◉ did not. */}
            {multi ? (
              <View
                style={[
                  styles.checkbox,
                  { borderColor: on ? QUESTION_ACCENT : theme.borderStrong, backgroundColor: on ? QUESTION_ACCENT : 'transparent' },
                ]}
              >
                {on ? <Icon icon={Check} size="xs" color="#ffffff" /> : null}
              </View>
            ) : (
              <View style={[styles.radio, { borderColor: on ? QUESTION_ACCENT : theme.borderStrong }]}>
                {on ? <View style={styles.radioDot} /> : null}
              </View>
            )}
            <View style={styles.optionText}>
              <ThemedText>{option.label}</ThemedText>
              {option.description ? (
                <ThemedText type="small" style={styles.note}>
                  {option.description}
                </ThemedText>
              ) : null}
            </View>
          </Pressable>
        );
      })}

      {question.other_allowed ? (
        <TextInput
          value={answer?.custom ?? ''}
          editable={!disabled}
          onChangeText={(text) => onChange({ custom: text, selected: [] })}
          placeholder="Something else…"
          placeholderTextColor={theme.textSecondary}
          style={[styles.input, { backgroundColor: theme.background, color: theme.text }]}
          multiline
        />
      ) : null}
    </View>
  );
}

const styles = StyleSheet.create({
  block: { gap: Spacing.one, paddingTop: Spacing.one },
  header: { opacity: 0.8, textTransform: 'uppercase' },
  body: { lineHeight: 20 },
  option: {
    flexDirection: 'row',
    alignItems: 'flex-start',
    gap: Spacing.two,
    borderWidth: 1,
    borderRadius: Spacing.two,
    padding: Spacing.two,
  },
  // 18px controls, nudged down 2px to sit on the label's cap height rather than
  // its line box.
  checkbox: {
    width: 18,
    height: 18,
    borderRadius: Radius.sm,
    borderWidth: 2,
    alignItems: 'center',
    justifyContent: 'center',
    marginTop: 2,
  },
  radio: {
    width: 18,
    height: 18,
    borderRadius: Radius.full,
    borderWidth: 2,
    alignItems: 'center',
    justifyContent: 'center',
    marginTop: 2,
  },
  radioDot: { width: 8, height: 8, borderRadius: Radius.full, backgroundColor: QUESTION_ACCENT },
  optionText: { flex: 1, gap: Spacing.half },
  input: {
    minHeight: 44,
    borderRadius: Spacing.two,
    paddingHorizontal: Spacing.three,
    paddingVertical: Spacing.two,
    fontSize: 16,
  },
  dim: { opacity: 0.4 },
  note: { opacity: 0.8 },
});
