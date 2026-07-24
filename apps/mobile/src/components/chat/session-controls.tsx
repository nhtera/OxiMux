import { ChevronDown, Shield, Sparkles } from 'lucide-react-native';
import type { Choice } from 'oximux-core';
import { useEffect, useState } from 'react';
import { StyleSheet } from 'react-native';

import { ChoicePicker } from '@/components/chat/choice-picker';
import { Chip } from '@/components/ui/badge';
import { useChoices } from '@/native/choices';

/**
 * The composer's model and permission-mode controls.
 *
 * A hook rather than a component because its two halves belong in different
 * places in the tree: the chips sit *inside* the composer's control row, while
 * the pickers are full-width bottom sheets that must be anchored to the screen.
 * A sheet positions itself absolutely against its parent, so rendering it beside
 * the chips would pin it to a 32pt-tall row instead of the window.
 *
 * Each chip is omitted when its backend offers no options — a fix-at-spawn agent
 * with an empty catalog, or one before its handshake completes. Showing a chip
 * that opens an empty sheet, or one that always fails, would be worse than not
 * offering the control at all.
 *
 * Errors go to the screen's own error banner via `onError` rather than to a
 * banner wedged between the input and the buttons, which used to push the whole
 * composer up over the keyboard the moment a switch was refused.
 */
export function useSessionControls(sessionId: string, onError: (message: string) => void) {
  const { choices, busy, error, pick, dismissError } = useChoices(sessionId);
  const [open, setOpen] = useState<'model' | 'mode' | null>(null);

  // Forwarded rather than swallowed: the user tapped something and is owed an
  // answer. A fix-at-spawn backend refuses here, and the host's message explains
  // why rather than leaving a control that seems broken. Cleared locally once
  // handed over so the same failure is not re-reported on every render.
  useEffect(() => {
    if (!error) return;
    onError(error);
    dismissError();
  }, [error, onError, dismissError]);

  const hasModels = choices.models.length > 0;
  const hasModes = choices.modes.length > 0;

  const chips = (
    <>
      {hasModels ? (
        <Chip
          icon={Sparkles}
          trailing={ChevronDown}
          label={labelFor(choices.models, choices.currentModel) ?? 'Model'}
          busy={busy}
          onPress={() => setOpen('model')}
        />
      ) : null}
      {hasModes ? (
        <Chip
          icon={Shield}
          trailing={ChevronDown}
          label={labelFor(choices.modes, choices.currentMode) ?? 'Mode'}
          busy={busy}
          // Never shrinks. Mode labels are one short word and model ids are long
          // ("claude-opus-5[1m]"), so sharing the squeeze evenly truncated the
          // mode chip to a single letter while the model chip stayed readable —
          // the opposite of useful. The model chip absorbs the whole shortfall.
          style={styles.fixed}
          onPress={() => setOpen('mode')}
        />
      ) : null}
    </>
  );

  const pickers = (
    <>
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
    </>
  );

  return { chips: hasModels || hasModes ? chips : null, pickers };
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
  fixed: { flexShrink: 0 },
});
