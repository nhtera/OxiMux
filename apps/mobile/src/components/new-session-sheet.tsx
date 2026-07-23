import { useState } from 'react';
import { ActivityIndicator, StyleSheet, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Button } from '@/components/ui/button';
import { ErrorBanner } from '@/components/ui/error-banner';
import { BottomSheetTextInput, Sheet } from '@/components/ui/sheet';
import { Radius, Spacing } from '@/constants/theme';
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
    <Sheet visible={visible} onClose={onClose}>
      <ThemedText type="smallBold">New session</ThemedText>

      <BottomSheetTextInput
        value={cwd}
        onChangeText={setCwd}
        placeholder="/path/on/the/desktop"
        placeholderTextColor={theme.textSecondary}
        autoCapitalize="none"
        autoCorrect={false}
        // Paths are not sentences; the default keyboard's capitalisation and
        // autocorrect actively fight them.
        keyboardType="default"
        style={[styles.input, { backgroundColor: theme.backgroundElement, color: theme.text }]}
      />
      <ThemedText type="small" style={styles.hint}>
        A folder on the desktop running OxiMux — not on this phone.
      </ThemedText>

      {error ? <ErrorBanner message={error} /> : null}

      <View style={styles.actions}>
        {busy ? <ActivityIndicator /> : null}
        <View style={styles.spacer} />
        <Button label="Cancel" variant="secondary" disabled={busy} onPress={onClose} />
        <Button
          label="Start"
          variant="primary"
          disabled={busy || trimmed.length === 0}
          onPress={() => onCreate(trimmed)}
        />
      </View>
    </Sheet>
  );
}

const styles = StyleSheet.create({
  input: {
    borderRadius: Radius.md,
    paddingHorizontal: Spacing.three,
    paddingVertical: Spacing.two,
    fontSize: 16,
  },
  hint: { opacity: 0.7 },
  actions: { flexDirection: 'row', alignItems: 'center', gap: Spacing.two, paddingTop: Spacing.two },
  spacer: { flex: 1 },
});
