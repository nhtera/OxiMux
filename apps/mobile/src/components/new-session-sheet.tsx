import { useState } from 'react';
import { ActivityIndicator, Modal, Pressable, StyleSheet, TextInput, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';

/**
 * Start a session on the desktop, from a path typed on the phone.
 *
 * A typed path is a poor phone interaction and this does not pretend otherwise —
 * but the alternative was worse. Offering a picker would mean browsing the
 * desktop's filesystem over the wire, which is a whole capability of its own and
 * a much larger surface than typing a path the user already knows. Recent paths
 * would help and are worth adding once there is a list to draw from; there is
 * none today.
 *
 * The path is the **desktop's**, which is the mistake a user is most likely to
 * make from a phone, so the field says so rather than leaving it to be inferred
 * from a failure.
 */
export function NewSessionSheet({
  visible,
  busy,
  error,
  onCreate,
  onClose,
}: {
  visible: boolean;
  busy: boolean;
  error?: string;
  onCreate: (cwd: string) => void;
  onClose: () => void;
}) {
  const theme = useTheme();
  const [cwd, setCwd] = useState('');
  const trimmed = cwd.trim();

  return (
    <Modal visible={visible} transparent animationType="slide" onRequestClose={onClose}>
      <Pressable style={styles.backdrop} onPress={onClose}>
        <Pressable style={[styles.sheet, { backgroundColor: theme.background }]} onPress={() => {}}>
          <ThemedText type="smallBold">New session</ThemedText>

          <TextInput
            value={cwd}
            onChangeText={setCwd}
            placeholder="/path/on/the/desktop"
            placeholderTextColor={theme.textSecondary}
            autoCapitalize="none"
            autoCorrect={false}
            // Paths are not sentences; the default keyboard's capitalisation and
            // autocorrect actively fight them.
            keyboardType="default"
            style={[
              styles.input,
              { backgroundColor: theme.backgroundElement, color: theme.text },
            ]}
          />
          <ThemedText type="small" style={styles.hint}>
            A folder on the desktop running OxiMux — not on this phone.
          </ThemedText>

          {error ? (
            <ThemedText type="small" style={styles.error} numberOfLines={3}>
              {error}
            </ThemedText>
          ) : null}

          <View style={styles.actions}>
            {busy ? <ActivityIndicator /> : null}
            <View style={styles.spacer} />
            <Pressable onPress={onClose} disabled={busy} style={styles.button}>
              <ThemedText type="code">Cancel</ThemedText>
            </Pressable>
            <Pressable
              onPress={() => onCreate(trimmed)}
              disabled={busy || trimmed.length === 0}
              style={[
                styles.button,
                { backgroundColor: theme.backgroundSelected },
                (busy || trimmed.length === 0) && styles.disabled,
              ]}
            >
              <ThemedText type="code">Start</ThemedText>
            </Pressable>
          </View>
        </Pressable>
      </Pressable>
    </Modal>
  );
}

const styles = StyleSheet.create({
  backdrop: { flex: 1, justifyContent: 'flex-end', backgroundColor: 'rgba(0,0,0,0.4)' },
  sheet: {
    borderTopLeftRadius: Spacing.three,
    borderTopRightRadius: Spacing.three,
    padding: Spacing.four,
    gap: Spacing.two,
  },
  input: {
    borderRadius: Spacing.two,
    paddingHorizontal: Spacing.three,
    paddingVertical: Spacing.two,
    fontSize: 16,
  },
  hint: { opacity: 0.7 },
  error: { color: '#F85149' },
  actions: { flexDirection: 'row', alignItems: 'center', gap: Spacing.two, paddingTop: Spacing.two },
  spacer: { flex: 1 },
  button: {
    paddingVertical: Spacing.two,
    paddingHorizontal: Spacing.three,
    borderRadius: Spacing.two,
  },
  disabled: { opacity: 0.4 },
});
