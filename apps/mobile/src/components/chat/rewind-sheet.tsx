import { useMemo, useState } from 'react';
import { ActivityIndicator, Pressable, StyleSheet, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Button } from '@/components/ui/button';
import { ErrorBanner } from '@/components/ui/error-banner';
import { Sheet } from '@/components/ui/sheet';
import { Spacing } from '@/constants/theme';
import { impact } from '@/native/haptics';
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
    <Sheet visible={visible} onClose={close}>
      {confirming ? (
        <Confirm
          target={confirming}
          busy={busy}
          error={error}
          onBack={() => setConfirming(null)}
          onGo={() => {
            impact();
            onRewind(confirming.ordinal);
          }}
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
            // Newest first: the turn someone wants to undo is almost always a
            // recent one, and a long conversation would otherwise bury it.
            [...targets].reverse().map((target) => (
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
            ))
          )}
          <Button label="Cancel" variant="secondary" onPress={close} />
        </>
      )}
    </Sheet>
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

      {error ? <ErrorBanner message={error} /> : null}

      <View style={styles.actions}>
        {busy ? <ActivityIndicator /> : null}
        <View style={styles.spacer} />
        <Button label="Back" variant="secondary" disabled={busy} onPress={onBack} />
        <Button label="Rewind" variant="danger" disabled={busy} onPress={onGo} />
      </View>
    </>
  );
}

const styles = StyleSheet.create({
  row: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: Spacing.two,
    paddingVertical: Spacing.three,
    borderBottomWidth: StyleSheet.hairlineWidth,
  },
  rowText: { flex: 1 },
  hint: { opacity: 0.7 },
  actions: { flexDirection: 'row', alignItems: 'center', gap: Spacing.two, paddingTop: Spacing.two },
  spacer: { flex: 1 },
});
