import type { Recurrence } from 'oximux-core';
import { useState } from 'react';
import {
  ActivityIndicator,
  Modal,
  Pressable,
  ScrollView,
  StyleSheet,
  TextInput,
  View,
} from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import type { ScheduleDraft } from '@/native/use-schedules';

import { RecurrencePicker, defaultRecurrence } from './recurrence-picker';

/**
 * Create a schedule from the phone.
 *
 * The most important thing this sheet does is tell the truth about a limit the
 * feature's name hides: a scheduled run fires only while the desktop app is
 * running. "Cron agent runs" reads like OS-level scheduling to most people, and
 * a run that silently never happened because the Mac was asleep is
 * indistinguishable from a broken feature — so the constraint is stated here,
 * before the user commits, not discovered after a missed run.
 *
 * `cwd` is a path on the **desktop**, the same poor-but-unavoidable interaction
 * `NewSessionSheet` explains: a picker would mean browsing the desktop's
 * filesystem over the wire, a far larger surface than typing a path the user
 * already knows.
 */
export function ScheduleFormSheet({
  visible,
  busy,
  error,
  onCreate,
  onClose,
}: {
  visible: boolean;
  busy: boolean;
  error?: string;
  onCreate: (draft: ScheduleDraft) => void;
  onClose: () => void;
}) {
  const theme = useTheme();
  const [name, setName] = useState('');
  const [cwd, setCwd] = useState('');
  const [prompt, setPrompt] = useState('');
  const [recurrence, setRecurrence] = useState<Recurrence>(defaultRecurrence());

  const trimmedName = name.trim();
  const trimmedCwd = cwd.trim();
  const trimmedPrompt = prompt.trim();
  const ready = trimmedName.length > 0 && trimmedCwd.length > 0 && trimmedPrompt.length > 0;

  const submit = () => {
    if (!ready) return;
    onCreate({ name: trimmedName, cwd: trimmedCwd, prompt: trimmedPrompt, recurrence });
  };

  const field = [styles.input, { backgroundColor: theme.backgroundElement, color: theme.text }];

  return (
    <Modal visible={visible} transparent animationType="slide" onRequestClose={onClose}>
      <Pressable style={styles.backdrop} onPress={onClose}>
        <Pressable style={[styles.sheet, { backgroundColor: theme.background }]} onPress={() => {}}>
          <ScrollView
            contentContainerStyle={styles.body}
            keyboardShouldPersistTaps="handled"
            showsVerticalScrollIndicator={false}
          >
            <ThemedText type="smallBold">New schedule</ThemedText>

            <TextInput
              value={name}
              onChangeText={setName}
              placeholder="Name (e.g. Nightly report)"
              placeholderTextColor={theme.textSecondary}
              style={field}
            />

            <TextInput
              value={cwd}
              onChangeText={setCwd}
              placeholder="/path/on/the/desktop"
              placeholderTextColor={theme.textSecondary}
              autoCapitalize="none"
              autoCorrect={false}
              style={field}
            />
            <ThemedText type="small" style={styles.hint}>
              A folder on the desktop running OxiMux — not on this phone.
            </ThemedText>

            <TextInput
              value={prompt}
              onChangeText={setPrompt}
              placeholder="What should the agent do?"
              placeholderTextColor={theme.textSecondary}
              multiline
              style={[...field, styles.prompt]}
            />

            <ThemedText type="small" style={styles.label}>
              Repeats
            </ThemedText>
            <RecurrencePicker value={recurrence} onChange={setRecurrence} />

            {/* The load-bearing disclaimer — see the component doc. */}
            <ThemedText type="small" style={styles.warning}>
              Runs only while the desktop app is open. OxiMux can keep the Mac
              awake, but it cannot wake a sleeping Mac or reopen a quit app, so a
              run scheduled for a time the app was closed will not happen.
            </ThemedText>

            {error ? (
              <ThemedText type="small" style={styles.error} numberOfLines={4}>
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
                onPress={submit}
                disabled={busy || !ready}
                style={[
                  styles.button,
                  { backgroundColor: theme.backgroundSelected },
                  (busy || !ready) && styles.disabled,
                ]}
              >
                <ThemedText type="code">Create</ThemedText>
              </Pressable>
            </View>
          </ScrollView>
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
    // Capped so a tall keyboard plus the recurrence controls still leaves the
    // backdrop tappable to dismiss.
    maxHeight: '90%',
  },
  body: { padding: Spacing.four, gap: Spacing.two },
  input: {
    borderRadius: Spacing.two,
    paddingHorizontal: Spacing.three,
    paddingVertical: Spacing.two,
    fontSize: 16,
  },
  prompt: { minHeight: 72, textAlignVertical: 'top' },
  hint: { opacity: 0.7 },
  label: { opacity: 0.7, paddingTop: Spacing.two },
  warning: { opacity: 0.85, paddingTop: Spacing.two, lineHeight: 18 },
  error: { color: '#F85149' },
  actions: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: Spacing.two,
    paddingTop: Spacing.three,
  },
  spacer: { flex: 1 },
  button: {
    paddingVertical: Spacing.two,
    paddingHorizontal: Spacing.three,
    borderRadius: Spacing.two,
  },
  disabled: { opacity: 0.4 },
});
