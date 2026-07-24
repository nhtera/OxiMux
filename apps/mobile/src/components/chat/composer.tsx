import { ArrowUp, CornerDownRight, ImagePlus } from 'lucide-react-native';
import { useRef, useState } from 'react';
import { Pressable, StyleSheet, TextInput, View } from 'react-native';

import { AttachmentStrip } from '@/components/chat/attachment-strip';
import { MicButton } from '@/components/chat/mic-button';
import { filterCommands, SlashPalette, slashQuery } from '@/components/chat/slash-palette';
import { Button } from '@/components/ui/button';
import { Icon } from '@/components/ui/icon';
import { IconButton } from '@/components/ui/icon-button';
import { Radius, Spacing } from '@/constants/theme';
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
   * The model/mode chips. Optional so the composer still renders for a backend
   * that offers no choices, or before the catalog has been fetched.
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
 * The composer. While a turn is in flight the primary action becomes **Steer**
 * rather than Send: typing mid-turn almost always means "also do this", and
 * sending would queue a whole new prompt to run after the current one instead of
 * guiding it. Stop sits next to it for the other intent.
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

  // A dictated transcript is inserted like typed text — it never auto-sends, so a
  // misheard word can be fixed before the prompt goes out. A separator is added
  // only when the box already has text not ending in whitespace, so back-to-back
  // dictations read as separate words rather than running together.
  const insertDictated = (dictated: string) => {
    setText((current) => {
      const needsSpace = current.length > 0 && !/\s$/.test(current);
      return `${current}${needsSpace ? ' ' : ''}${dictated}`;
    });
  };
  const dictation = useDictation({ onText: insertDictated, onError });

  const submit = async () => {
    if (!canSubmit) return;
    impact();
    setBusy(true);
    // Clear optimistically: the desktop echoes the prompt back as a real entry,
    // so leaving it in the box would read as "not sent" once it appears above.
    const queued = attachments;
    setText('');
    setAttachments([]);
    try {
      const sent = await (steering ? onSteer(trimmed) : onSend(trimmed, queued));
      // ...but put it back if it never landed. On a phone the link drops
      // routinely, and silently eating a typed prompt is worse than the error
      // alone: the user would have to retype it with nothing to copy from.
      // The images come back for the same reason — re-picking them from the
      // library is a longer detour than retyping a sentence.
      if (!sent) {
        setText((current) => (current.length > 0 ? current : trimmed));
        setAttachments((current) => (current.length > 0 ? current : queued));
      }
    } finally {
      setBusy(false);
    }
  };

  return (
    <View style={[styles.bar, { borderTopColor: theme.backgroundSelected }]}>
      <AttachmentStrip
        attachments={attachments}
        onRemove={(id) => setAttachments((current) => current.filter((a) => a.id !== id))}
      />

      {matches.length > 0 ? (
        <SlashPalette commands={matches} onSelect={chooseCommand} />
      ) : null}

      <TextInput
        ref={inputRef}
        value={text}
        onChangeText={setText}
        placeholder={steering ? 'Steer this turn…' : 'Send a prompt…'}
        placeholderTextColor={theme.textSecondary}
        autoCapitalize="sentences"
        multiline
        style={[
          styles.input,
          { backgroundColor: theme.backgroundElement, color: theme.text },
        ]}
      />

      {/* Behaviour controls sit below the input, matching where the desktop
          composer puts them — context above the input, behaviour below. */}
      {controls}

      <View style={styles.actions}>
        <IconButton
          icon={ImagePlus}
          onPress={attach}
          disabled={!canAttach}
          accessibilityLabel={room === 0 ? `Attachment limit reached (${MAX_ATTACHMENTS} max)` : 'Attach an image'}
        />

        {/* Dictation only appears while connected — the desktop is what
            transcribes, so a disconnected phone has nothing to offer here. */}
        {dictation.available ? (
          <MicButton
            phase={dictation.phase}
            level={dictation.level}
            onStart={dictation.start}
            onStop={dictation.stop}
          />
        ) : null}

        <View style={styles.spacer} />

        {turnActive ? <Button label="Stop" variant="danger" onPress={onCancel} /> : null}

        {/* Icon-first send: a filled circle that lights to the accent once there is
            something to send. Mid-turn the intent is Steer, not Send, so the glyph
            swaps to make that unmistakable. */}
        <Pressable
          onPress={submit}
          disabled={!canSubmit}
          accessibilityRole="button"
          accessibilityLabel={steering ? 'Steer this turn' : 'Send'}
          style={[styles.send, { backgroundColor: canSubmit ? theme.accent : theme.surface3 }]}
        >
          <Icon
            icon={steering ? CornerDownRight : ArrowUp}
            size="md"
            color={canSubmit ? theme.accentText : theme.textMuted}
          />
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
  actions: { flexDirection: 'row', alignItems: 'center', gap: Spacing.two },
  spacer: { flex: 1 },
  send: {
    width: 40,
    height: 40,
    borderRadius: Radius.full,
    alignItems: 'center',
    justifyContent: 'center',
  },
});
