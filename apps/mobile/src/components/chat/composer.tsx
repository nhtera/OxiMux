import { useState } from 'react';
import { Pressable, StyleSheet, TextInput, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';

type Props = {
  /** True while a turn is running — swaps Send for Steer and offers Stop. */
  turnActive: boolean;
  /** Resolves `false` when the prompt did not reach the desktop. */
  onSend: (text: string) => Promise<boolean>;
  onSteer: (text: string) => Promise<boolean>;
  onCancel: () => Promise<unknown>;
};

/**
 * The composer. While a turn is in flight the primary action becomes **Steer**
 * rather than Send: typing mid-turn almost always means "also do this", and
 * sending would queue a whole new prompt to run after the current one instead of
 * guiding it. Stop sits next to it for the other intent.
 */
export function Composer({ turnActive, onSend, onSteer, onCancel }: Props) {
  const theme = useTheme();
  const [text, setText] = useState('');
  const [busy, setBusy] = useState(false);
  const trimmed = text.trim();
  const canSubmit = trimmed.length > 0 && !busy;

  const submit = async () => {
    if (!canSubmit) return;
    setBusy(true);
    // Clear optimistically: the desktop echoes the prompt back as a real entry,
    // so leaving it in the box would read as "not sent" once it appears above.
    setText('');
    try {
      const sent = await (turnActive ? onSteer(trimmed) : onSend(trimmed));
      // ...but put it back if it never landed. On a phone the link drops
      // routinely, and silently eating a typed prompt is worse than the error
      // alone: the user would have to retype it with nothing to copy from.
      if (!sent) setText((current) => (current.length > 0 ? current : trimmed));
    } finally {
      setBusy(false);
    }
  };

  return (
    <View style={[styles.bar, { borderTopColor: theme.backgroundSelected }]}>
      <TextInput
        value={text}
        onChangeText={setText}
        placeholder={turnActive ? 'Steer this turn…' : 'Send a prompt…'}
        placeholderTextColor={theme.textSecondary}
        autoCapitalize="sentences"
        multiline
        style={[
          styles.input,
          { backgroundColor: theme.backgroundElement, color: theme.text },
        ]}
      />

      <View style={styles.actions}>
        {turnActive ? (
          <Pressable onPress={onCancel} style={[styles.button, styles.stop]}>
            <ThemedText type="code">Stop</ThemedText>
          </Pressable>
        ) : null}

        <Pressable
          onPress={submit}
          disabled={!canSubmit}
          style={[
            styles.button,
            { backgroundColor: theme.backgroundSelected },
            !canSubmit && styles.disabled,
          ]}
        >
          <ThemedText type="code">{turnActive ? 'Steer' : 'Send'}</ThemedText>
        </Pressable>
      </View>
    </View>
  );
}

const styles = StyleSheet.create({
  bar: {
    borderTopWidth: StyleSheet.hairlineWidth,
    padding: Spacing.three,
    gap: Spacing.two,
  },
  input: {
    minHeight: 44,
    maxHeight: 140,
    borderRadius: Spacing.two,
    paddingHorizontal: Spacing.three,
    paddingVertical: Spacing.two,
    fontSize: 16,
  },
  actions: { flexDirection: 'row', justifyContent: 'flex-end', gap: Spacing.two },
  button: {
    paddingVertical: Spacing.two,
    paddingHorizontal: Spacing.four,
    borderRadius: Spacing.two,
  },
  stop: { borderWidth: 1, borderColor: '#F85149' },
  disabled: { opacity: 0.4 },
});
