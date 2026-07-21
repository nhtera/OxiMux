import { useMemo, useState } from 'react';
import { ActivityIndicator, Modal, Pressable, ScrollView, StyleSheet, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { rewindTargets, targetLabel, type RewindTarget } from '@/native/rewind';
import type { ThreadEntry } from '@/native/thread';

/**
 * Pick a turn to rewind to, then confirm.
 *
 * Two steps on purpose. A rewind destroys conversation the user may not be able
 * to reconstruct, and a single-tap list would put an irreversible action one
 * mis-tap away on a device people use while walking. The confirm step states
 * the blast radius — how many entries go — before anything happens.
 */
export function RewindSheet({
  visible,
  entries,
  busy,
  error,
  onRewind,
  onClose,
}: {
  visible: boolean;
  entries: ThreadEntry[];
  busy: boolean;
  error?: string;
  onRewind: (ordinal: number) => void;
  onClose: () => void;
}) {
  const theme = useTheme();
  const [confirming, setConfirming] = useState<RewindTarget | null>(null);
  const targets = useMemo(() => rewindTargets(entries), [entries]);

  const close = () => {
    setConfirming(null);
    onClose();
  };

  return (
    <Modal visible={visible} transparent animationType="slide" onRequestClose={close}>
      <Pressable style={styles.backdrop} onPress={close}>
        <Pressable style={[styles.sheet, { backgroundColor: theme.background }]} onPress={() => {}}>
          {confirming ? (
            <Confirm
              target={confirming}
              busy={busy}
              error={error}
              onBack={() => setConfirming(null)}
              onGo={() => onRewind(confirming.ordinal)}
            />
          ) : (
            <>
              <ThemedText type="smallBold">Rewind to a message</ThemedText>
              <ThemedText type="small" style={styles.hint}>
                The chosen message and everything after it are removed.
              </ThemedText>
              {targets.length === 0 ? (
                <ThemedText type="small" style={styles.hint}>
                  Nothing to rewind to yet.
                </ThemedText>
              ) : (
                <ScrollView style={styles.list}>
                  {/* Newest first: the turn someone wants to undo is almost
                      always a recent one, and a long conversation would
                      otherwise bury it under scroll. */}
                  {[...targets].reverse().map((target) => (
                    <Pressable
                      key={target.ordinal}
                      onPress={() => setConfirming(target)}
                      style={[styles.row, { borderBottomColor: theme.backgroundElement }]}
                    >
                      <ThemedText numberOfLines={1} style={styles.rowText}>
                        {targetLabel(target)}
                      </ThemedText>
                      <ThemedText type="small" style={styles.hint}>
                        −{target.messagesRemoved}
                      </ThemedText>
                    </Pressable>
                  ))}
                </ScrollView>
              )}
              <Pressable onPress={close} style={styles.button}>
                <ThemedText type="code">Cancel</ThemedText>
              </Pressable>
            </>
          )}
        </Pressable>
      </Pressable>
    </Modal>
  );
}

function Confirm({
  target,
  busy,
  error,
  onBack,
  onGo,
}: {
  target: RewindTarget;
  busy: boolean;
  error?: string;
  onBack: () => void;
  onGo: () => void;
}) {
  const theme = useTheme();
  const count = target.messagesRemoved;

  return (
    <>
      <ThemedText type="smallBold">
        {/* The count, not a generic "are you sure" — it is the one fact that
            distinguishes undoing a stray message from discarding an hour. */}
        Remove {count} {count === 1 ? 'message' : 'messages'}?
      </ThemedText>
      <ThemedText type="small" style={styles.hint} numberOfLines={3}>
        Rewinding to “{targetLabel(target, 80)}”.
      </ThemedText>
      <ThemedText type="small" style={styles.hint}>
        {/* Said plainly because the phone cannot offer the files axis: a user
            who assumed a rewind also reverts their code would otherwise find
            out by looking at a repo that no longer matches the conversation. */}
        Files on disk are left as they are. Restoring files to a snapshot is only
        available on the desktop.
      </ThemedText>

      {error ? (
        <ThemedText type="small" style={styles.error} numberOfLines={3}>
          {error}
        </ThemedText>
      ) : null}

      <View style={styles.actions}>
        {busy ? <ActivityIndicator /> : null}
        <View style={styles.spacer} />
        <Pressable onPress={onBack} disabled={busy} style={styles.button}>
          <ThemedText type="code">Back</ThemedText>
        </Pressable>
        <Pressable
          onPress={onGo}
          disabled={busy}
          style={[styles.button, styles.danger, busy && styles.disabled]}
        >
          <ThemedText type="code" style={{ color: theme.text }}>
            Rewind
          </ThemedText>
        </Pressable>
      </View>
    </>
  );
}

const styles = StyleSheet.create({
  backdrop: { flex: 1, justifyContent: 'flex-end', backgroundColor: 'rgba(0,0,0,0.4)' },
  sheet: {
    borderTopLeftRadius: Spacing.three,
    borderTopRightRadius: Spacing.three,
    padding: Spacing.four,
    gap: Spacing.two,
    maxHeight: '70%',
  },
  list: { maxHeight: 280 },
  row: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: Spacing.two,
    paddingVertical: Spacing.three,
    borderBottomWidth: StyleSheet.hairlineWidth,
  },
  rowText: { flex: 1 },
  hint: { opacity: 0.7 },
  error: { color: '#F85149' },
  actions: { flexDirection: 'row', alignItems: 'center', gap: Spacing.two, paddingTop: Spacing.two },
  spacer: { flex: 1 },
  button: {
    paddingVertical: Spacing.two,
    paddingHorizontal: Spacing.three,
    borderRadius: Spacing.two,
  },
  danger: { backgroundColor: 'rgba(248,81,73,0.22)' },
  disabled: { opacity: 0.4 },
});
