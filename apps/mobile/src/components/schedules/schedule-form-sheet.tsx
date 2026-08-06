import type { Recurrence } from 'oximux-core';
import { useState } from 'react';
import { ActivityIndicator, StyleSheet, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Button } from '@/components/ui/button';
import { ErrorBanner } from '@/components/ui/error-banner';
import { BottomSheetTextInput, Sheet } from '@/components/ui/sheet';
import { Radius, Spacing } from '@/constants/theme';
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
    <Sheet visible={visible} onClose={onClose}>
      <ThemedText type="smallBold">New schedule</ThemedText>

      <BottomSheetTextInput
        value={name}
        onChangeText={setName}
        placeholder="Name (e.g. Nightly report)"
        placeholderTextColor={theme.textSecondary}
        style={field}
      />

      <BottomSheetTextInput
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

      <BottomSheetTextInput
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
        Runs only while an OxiMux host is running — the desktop app or a
        server. OxiMux cannot wake a sleeping machine or relaunch a quit host,
        so a run scheduled for a time no host was up will not happen.
      </ThemedText>

      {error ? <ErrorBanner message={error} /> : null}

      <View style={styles.actions}>
        {busy ? <ActivityIndicator /> : null}
        <View style={styles.spacer} />
        <Button label="Cancel" variant="secondary" disabled={busy} onPress={onClose} />
        <Button label="Create" variant="primary" disabled={busy || !ready} onPress={submit} />
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
  prompt: { minHeight: 72, textAlignVertical: 'top' },
  hint: { opacity: 0.7 },
  label: { opacity: 0.7, paddingTop: Spacing.two },
  warning: { opacity: 0.85, paddingTop: Spacing.two, lineHeight: 18 },
  actions: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: Spacing.two,
    paddingTop: Spacing.three,
  },
  spacer: { flex: 1 },
});
