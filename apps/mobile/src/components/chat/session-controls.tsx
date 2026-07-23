import type { Choice } from 'oximux-core';
import { useState } from 'react';
import { StyleSheet, View } from 'react-native';

import { ChoicePicker } from '@/components/chat/choice-picker';
import { Chip } from '@/components/ui/badge';
import { ErrorBanner } from '@/components/ui/error-banner';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { useChoices } from '@/native/choices';

/**
 * The model and permission-mode chips under the composer.
 *
 * Each chip is hidden when its backend offers no options — a fix-at-spawn agent
 * with an empty catalog, or one before its handshake completes. Showing a chip
 * that opens an empty sheet, or one that always fails, would be worse than not
 * offering the control at all.
 */
export function SessionControls({ sessionId }: { sessionId: string }) {
  const theme = useTheme();
  const { choices, busy, error, pick, dismissError } = useChoices(sessionId);
  const [open, setOpen] = useState<'model' | 'mode' | null>(null);

  const hasModels = choices.models.length > 0;
  const hasModes = choices.modes.length > 0;
  if (!hasModels && !hasModes) return null;

  return (
    <View style={styles.wrap}>
      <View style={styles.row}>
        {hasModels ? (
          <Chip
            label={labelFor(choices.models, choices.currentModel) ?? 'Model'}
            busy={busy}
            onPress={() => setOpen('model')}
          />
        ) : null}
        {hasModes ? (
          <Chip
            label={labelFor(choices.modes, choices.currentMode) ?? 'Mode'}
            busy={busy}
            onPress={() => setOpen('mode')}
          />
        ) : null}
      </View>

      {/* Surfaced rather than swallowed: the user tapped something and is owed
          an answer. A fix-at-spawn backend refuses here, and the host's message
          explains why rather than leaving a control that seems broken. */}
      {error ? <ErrorBanner message={error} onDismiss={dismissError} /> : null}

      <ChoicePicker
        title="Model"
        choices={choices.models}
        current={choices.currentModel}
        visible={open === 'model'}
        busy={busy}
        onPick={(id) => {
          setOpen(null);
          void pick('model', id);
        }}
        onClose={() => setOpen(null)}
      />
      <ChoicePicker
        title="Permission mode"
        choices={choices.modes}
        current={choices.currentMode}
        visible={open === 'mode'}
        busy={busy}
        onPick={(id) => {
          setOpen(null);
          void pick('mode', id);
        }}
        onClose={() => setOpen(null)}
      />

      {/* Themed border kept out of StyleSheet so the chips follow the theme. */}
      <View style={[styles.hairline, { borderColor: theme.backgroundSelected }]} />
    </View>
  );
}

/**
 * The human label for whatever is current.
 *
 * Falls back to the raw id when the host reports a current value the catalog
 * does not list — which happens for a model set outside the picker. Showing the
 * id is worse than showing a label but far better than showing nothing, since
 * the whole point of the chip is saying what is running.
 */
function labelFor(choices: Choice[], current?: string): string | undefined {
  if (!current) return undefined;
  return choices.find((c) => c.id === current)?.label ?? current;
}

const styles = StyleSheet.create({
  wrap: { gap: Spacing.one },
  row: { flexDirection: 'row', gap: Spacing.two, flexWrap: 'wrap' },
  hairline: { height: 0 },
});
