import { useRef, useState } from 'react';
import { Pressable, StyleSheet, TextInput, View } from 'react-native';

import { AttachmentStrip } from '@/components/chat/attachment-strip';
import { filterCommands, SlashPalette, slashQuery } from '@/components/chat/slash-palette';
import { ThemedText } from '@/components/themed-text';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { MAX_ATTACHMENTS, pickImages, type Attachment } from '@/native/attachments';

type Props = {
  /** True while a turn is running — swaps Send for Steer and offers Stop. */
  turnActive: boolean;
  /** The agent's slash commands, for the composer palette. Names only. */
  slashCommands?: string[];
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
  onSend,
  onSteer,
  onCancel,
  onError,
}: Props) {
  const theme = useTheme();
  const inputRef = useRef<TextInput>(null);
  const [text, setText] = useState('');
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

  const submit = async () => {
    if (!canSubmit) return;
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

      <View style={styles.actions}>
        <Pressable
          onPress={attach}
          disabled={!canAttach}
          accessibilityLabel="Attach an image"
          style={[styles.button, styles.attach, !canAttach && styles.disabled]}
        >
          <ThemedText type="code">
            {room === 0 ? `${MAX_ATTACHMENTS} max` : 'Attach'}
          </ThemedText>
        </Pressable>

        <View style={styles.spacer} />

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
          <ThemedText type="code">{steering ? 'Steer' : 'Send'}</ThemedText>
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
  attach: { borderWidth: StyleSheet.hairlineWidth, borderColor: '#8B949E' },
  button: {
    paddingVertical: Spacing.two,
    paddingHorizontal: Spacing.four,
    borderRadius: Spacing.two,
  },
  stop: { borderWidth: 1, borderColor: '#F85149' },
  disabled: { opacity: 0.4 },
});
