import { StyleSheet, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import type { DictationPhase } from '@/native/use-dictation';

type Props = {
  phase: DictationPhase;
  /** `0..1` input level, drives the meter while recording. */
  level: number;
  onStart: () => void;
  onStop: () => void;
};

/**
 * The composer's dictation control: tap to record, tap to stop. While recording
 * it shows a live level meter so the user can see the mic is hearing them;
 * during transcription it is a disabled "…" and the tap is ignored until the
 * transcript lands. `denied` reads as "Mic off" and a tap re-triggers the
 * permission path (which surfaces the Settings hint) rather than doing nothing.
 *
 * Rendered by the composer only while connected — a phone with no desktop to
 * transcribe for it never shows this button at all.
 */
export function MicButton({ phase, level, onStart, onStop }: Props) {
  const theme = useTheme();
  const recording = phase === 'recording';
  const transcribing = phase === 'transcribing';

  const onPress = () => {
    if (recording) onStop();
    else if (!transcribing) onStart();
  };

  const label =
    phase === 'recording'
      ? 'Stop'
      : phase === 'transcribing'
        ? '…'
        : phase === 'denied'
          ? 'Mic off'
          : 'Mic';

  return (
    <View
      // A plain View with an accessibility role rather than Pressable so the
      // meter and label sit in one tappable pill without nesting pressables.
      accessibilityRole="button"
      accessibilityLabel={
        recording ? 'Stop dictation' : transcribing ? 'Transcribing' : 'Dictate a prompt'
      }
      accessibilityState={{ disabled: transcribing, busy: transcribing }}
      onTouchEnd={onPress}
      style={[
        styles.button,
        {
          borderColor: recording ? theme.danger : theme.textSecondary,
          opacity: transcribing ? 0.5 : 1,
        },
      ]}
    >
      {recording ? (
        <View style={styles.meterTrack}>
          <View
            style={[
              styles.meterFill,
              { width: `${Math.round(Math.min(1, level) * 100)}%`, backgroundColor: theme.danger },
            ]}
          />
        </View>
      ) : null}
      <ThemedText type="code" style={recording ? { color: theme.danger } : undefined}>
        {label}
      </ThemedText>
    </View>
  );
}

const styles = StyleSheet.create({
  button: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: Spacing.two,
    paddingVertical: Spacing.two,
    paddingHorizontal: Spacing.three,
    borderRadius: Spacing.two,
    borderWidth: StyleSheet.hairlineWidth,
  },
  meterTrack: {
    width: 36,
    height: 4,
    borderRadius: 2,
    backgroundColor: 'rgba(248,81,73,0.25)',
    overflow: 'hidden',
  },
  meterFill: {
    height: 4,
    borderRadius: 2,
  },
});
