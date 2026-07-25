import { ArrowUp, CornerDownRight, ImagePlus, Square } from 'lucide-react-native';
import { useRef, useState } from 'react';
import { Pressable, StyleSheet, TextInput, View } from 'react-native';

import { AttachmentStrip } from '@/components/chat/attachment-strip';
import { DictationBar } from '@/components/chat/dictation-bar';
import { MicButton } from '@/components/chat/mic-button';
import { filterCommands, SlashPalette, slashQuery } from '@/components/chat/slash-palette';
import { ControlHeight, IconHitSlop } from '@/components/ui/control-geometry';
import { Icon } from '@/components/ui/icon';
import { Radius, Spacing, withAlpha } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { MAX_ATTACHMENTS, pickImages, type Attachment } from '@/native/attachments';
import { impact } from '@/native/haptics';
import { useDictation } from '@/native/use-dictation';

type Props = {
  /** True while a turn is running — swaps Send for Steer and offers Stop. */
  turnActive: boolean;
  /** The agent's slash commands, for the composer palette. Names only. */
  slashCommands?: string[];
  /**
   * The model/mode chips, rendered inline on the control row. Optional so the
   * composer still renders for a backend that offers no choices, or before the
   * catalog has been fetched.
   */
  controls?: React.ReactNode;
  /**
   * Text to prefill the input with — how an attached PR/issue reaches the
   * composer.
   *
   * Applied when its **value changes**, not on every render, so it seeds the
   * input once and then leaves the user's edits alone. A prefill that reasserted
   * itself would fight anyone typing after it.
   */
  draft?: string;
  /** Resolves `false` when the prompt did not reach the desktop. */
  onSend: (text: string, images: Attachment[]) => Promise<boolean>;
  onSteer: (text: string) => Promise<boolean>;
  onCancel: () => Promise<unknown>;
  /** Surfaced by the screen alongside its other action failures. */
  onError: (message: string) => void;
};

/**
 * The composer.
 *
 * One rounded dock holds everything that composes a prompt — queued images, the
 * text, and a single row of controls — so the whole thing reads as one object
 * rising out of the transcript. It replaced three stacked bands (input, then a
 * chip row, then a button row) that each drew their own box and left the bar
 * tall and unsettled with nothing in it.
 *
 * The row's order is by frequency, left to right: attach, what the turn will run
 * as (model, mode), then the two ways to finish — dictate and send. Everything in
 * it is a glyph on the same circular step, so the eye lands on the one filled
 * button and the rest recede.
 *
 * Dictating replaces the dock's contents outright — see [`DictationBar`]. Nothing
 * in the row can be used while the microphone is open, so the row goes away.
 *
 * While a turn is in flight the primary action becomes **Steer** rather than
 * Send: typing mid-turn almost always means "also do this", and sending would
 * queue a whole new prompt to run after the current one instead of guiding it.
 * Stop sits next to it for the other intent.
 *
 * Attachments override that: steering carries text only on the wire, so a turn
 * running with images queued still sends. Silently dropping photos the user
 * deliberately attached would be the worse surprise of the two.
 */
export function Composer({
  turnActive,
  slashCommands = [],
  controls,
  draft,
  onSend,
  onSteer,
  onCancel,
  onError,
}: Props) {
  const theme = useTheme();
  const inputRef = useRef<TextInput>(null);
  // Seeded from `draft` at mount, which is the common case for an attachment:
  // the forge screen navigates here with the text already composed, so the
  // composer is constructed holding it rather than receiving it as a change.
  const [text, setText] = useState(draft ?? '');
  // Seed from `draft` only when it actually changes — React's adjust-state-
  // during-render pattern, which needs state rather than a ref (a ref written
  // during render is not safe under concurrent rendering).
  //
  // Comparing against the last *applied* draft rather than against `text` is
  // what lets the user edit or clear a prefilled draft without it snapping back
  // on the next render.
  const [lastDraft, setLastDraft] = useState(draft);
  if (draft !== undefined && draft !== lastDraft) {
    setLastDraft(draft);
    setText(draft);
  }
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const [busy, setBusy] = useState(false);
  const trimmed = text.trim();

  const query = slashQuery(text);
  const matches = query === undefined ? [] : filterCommands(slashCommands, query);

  const chooseCommand = (command: string) => {
    // Trailing space both dismisses the palette (a space ends the command name)
    // and puts the cursor where arguments go, so the next keystroke is useful.
    setText(`/${command} `);
    inputRef.current?.focus();
  };
  const steering = turnActive && attachments.length === 0;
  const canSubmit = (trimmed.length > 0 || attachments.length > 0) && !busy;
  const room = MAX_ATTACHMENTS - attachments.length;
  // Picking during an in-flight send would race the restore-on-failure path:
  // the newly picked images would be the ones kept, and the ones that failed to
  // send would be the ones dropped — the opposite of what the restore is for.
  const canAttach = room > 0 && !busy;

  const attach = async () => {
    try {
      const picked = await pickImages(room);
      if (picked.length > 0) setAttachments((current) => [...current, ...picked]);
    } catch (e) {
      onError(e instanceof Error ? e.message : 'Could not open the photo library.');
    }
  };

  /**
   * A transcript always joins what is already in the box rather than replacing
   * it, so "type a bit, dictate the rest" works and back-to-back dictations do
   * not run together. The separator is added only when the tail is not already
   * whitespace.
   */
  const joinDictated = (current: string, dictated: string) =>
    `${current}${current.length > 0 && !/\s$/.test(current) ? ' ' : ''}${dictated}`;

  /**
   * The one way a prompt leaves the composer, whether it was typed or spoken.
   *
   * Takes what to send explicitly rather than reading state, because a dictated
   * send has to carry a transcript that has not been committed to the box yet —
   * routing it through `setText` first would mean sending a render behind.
   */
  const dispatch = async (body: string, queued: Attachment[]) => {
    const outgoing = body.trim();
    if (busy || (outgoing.length === 0 && queued.length === 0)) return;
    impact();
    setBusy(true);
    // Clear optimistically: the desktop echoes the prompt back as a real entry,
    // so leaving it in the box would read as "not sent" once it appears above.
    setText('');
    setAttachments([]);
    try {
      // Attachments force a real send: steering carries text only on the wire, so
      // a turn running with images queued still sends rather than dropping them.
      const sent = await (turnActive && queued.length === 0
        ? onSteer(outgoing)
        : onSend(outgoing, queued));
      // ...but put it back if it never landed. On a phone the link drops
      // routinely, and silently eating a typed prompt is worse than the error
      // alone: the user would have to retype it with nothing to copy from.
      // The images come back for the same reason — re-picking them from the
      // library is a longer detour than retyping a sentence.
      if (!sent) {
        setText((current) => (current.length > 0 ? current : outgoing));
        setAttachments((current) => (current.length > 0 ? current : queued));
      }
    } finally {
      setBusy(false);
    }
  };

  const dictation = useDictation({
    onText: (dictated) => setText((current) => joinDictated(current, dictated)),
    // Dictating and then choosing Send goes out through the same path as a typed
    // prompt — including whatever was already typed and attached, which is the
    // only reading of "send" that does not silently drop work.
    onSubmit: (dictated) => void dispatch(joinDictated(text, dictated), attachments),
    onError,
  });

  const submit = () => void dispatch(text, attachments);

  const phase = dictation.phase;

  // The dock keeps its shape and swaps its contents, so starting dictation reads
  // as the composer changing mode rather than as one control being replaced by a
  // different bar that happens to sit in the same place.
  if (phase === 'recording' || phase === 'transcribing') {
    return (
      <View style={styles.bar}>
        <View style={[styles.dock, { backgroundColor: theme.surface1, borderColor: theme.border }]}>
          <DictationBar
            phase={phase}
            levels={dictation.levels}
            onCancel={() => void dictation.cancel()}
            onStop={() => void dictation.stop('insert')}
            onSend={() => void dictation.stop('send')}
          />
        </View>
      </View>
    );
  }

  return (
    <View style={styles.bar}>
      {/* The palette floats above the dock rather than inside it: it is a list of
          candidates for what is being typed, not part of the prompt. */}
      {matches.length > 0 ? (
        <SlashPalette commands={matches} onSelect={chooseCommand} />
      ) : null}

      <View style={[styles.dock, { backgroundColor: theme.surface1, borderColor: theme.border }]}>
        <AttachmentStrip
          attachments={attachments}
          onRemove={(id) => setAttachments((current) => current.filter((a) => a.id !== id))}
        />

        <TextInput
          ref={inputRef}
          value={text}
          onChangeText={setText}
          placeholder={steering ? 'Steer this turn…' : 'Send a prompt…'}
          placeholderTextColor={theme.textMuted}
          autoCapitalize="sentences"
          multiline
          // No fill of its own — the dock is the surface, and a second box inside
          // it was what made the old bar read as two unrelated fields.
          style={[styles.input, { color: theme.text }]}
        />

        <View style={styles.row}>
          <Pressable
            onPress={attach}
            disabled={!canAttach}
            hitSlop={IconHitSlop}
            accessibilityRole="button"
            accessibilityLabel={
              room === 0 ? `Attachment limit reached (${MAX_ATTACHMENTS} max)` : 'Attach an image'
            }
            style={({ pressed }) => [
              styles.circle,
              { backgroundColor: theme.surface2 },
              pressed && styles.pressed,
              !canAttach && styles.disabled,
            ]}
          >
            <Icon icon={ImagePlus} size="lg" color={theme.textSecondary} />
          </Pressable>

          {/* Behaviour controls sit with the send button rather than on a band of
              their own — they describe the turn that button will start. Given the
              leftover width so a long model name truncates instead of crowding
              out the actions. */}
          <View style={styles.controls}>{controls}</View>

          {/* Dictation only appears while connected — the desktop is what
              transcribes, so a disconnected phone has nothing to offer here. */}
          {dictation.available ? (
            <MicButton
              denied={dictation.phase === 'denied'}
              onStart={() => void dictation.start()}
            />
          ) : null}

          {/* Stop is a peer of send, not a red text button: mid-turn the two are
              the same kind of decision, and the label was the widest thing in the
              row for the one state where room is tightest. */}
          {turnActive ? (
            <Pressable
              onPress={onCancel}
              hitSlop={IconHitSlop}
              accessibilityRole="button"
              accessibilityLabel="Stop this turn"
              style={({ pressed }) => [
                styles.circle,
                { backgroundColor: withAlpha(theme.danger, 0.16) },
                pressed && styles.pressed,
              ]}
            >
              <Icon icon={Square} size="md" color={theme.danger} />
            </Pressable>
          ) : null}

          {/* Icon-first send: a filled circle that lights to the accent once there is
              something to send. Mid-turn the intent is Steer, not Send, so the glyph
              swaps to make that unmistakable. */}
          <Pressable
            onPress={submit}
            disabled={!canSubmit}
            accessibilityRole="button"
            accessibilityLabel={steering ? 'Steer this turn' : 'Send'}
            style={({ pressed }) => [
              styles.circle,
              { backgroundColor: canSubmit ? theme.accent : theme.surface3 },
              pressed && styles.pressed,
            ]}
          >
            <Icon
              icon={steering ? CornerDownRight : ArrowUp}
              size="lg"
              color={canSubmit ? theme.accentText : theme.textMuted}
            />
          </Pressable>
        </View>
      </View>
    </View>
  );
}

const styles = StyleSheet.create({
  bar: {
    paddingHorizontal: Spacing.two,
    paddingTop: Spacing.two,
    paddingBottom: Spacing.one,
    gap: Spacing.two,
  },
  dock: {
    borderWidth: StyleSheet.hairlineWidth,
    // Large enough to read as a capsule when the input is one line, and still
    // sane once it has grown to its cap.
    borderRadius: Radius.xl + Spacing.two,
    padding: Spacing.two,
    gap: Spacing.two,
  },
  input: {
    // One comfortable line at rest; grows to roughly six before it scrolls, which
    // is as much of the screen as the keyboard leaves.
    minHeight: ControlHeight.compact,
    maxHeight: 140,
    paddingHorizontal: Spacing.two,
    // iOS multiline inputs sit flush to the top without this and read as
    // mis-aligned against the placeholder.
    paddingTop: Spacing.one,
    paddingBottom: 0,
    fontSize: 16,
  },
  row: { flexDirection: 'row', alignItems: 'center', gap: Spacing.two },
  // `flex: 1` doubles as the spacer that pushes mic/send to the right edge, so
  // an agent offering no chips still lays out correctly.
  controls: { flex: 1, flexDirection: 'row', alignItems: 'center', gap: Spacing.one },
  circle: {
    width: ControlHeight.compact,
    height: ControlHeight.compact,
    borderRadius: Radius.full,
    alignItems: 'center',
    justifyContent: 'center',
  },
  pressed: { opacity: 0.6 },
  disabled: { opacity: 0.4 },
});
