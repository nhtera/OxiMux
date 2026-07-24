import { Mic, MicOff, Square } from 'lucide-react-native';
import { ActivityIndicator, Pressable, StyleSheet, View } from 'react-native';

import { ControlHeight, IconHitSlop } from '@/components/ui/control-geometry';
import { Icon } from '@/components/ui/icon';
import { Radius, withAlpha } from '@/constants/theme';
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
 * The composer's dictation control: tap to record, tap to stop.
 *
 * Icon-only and circular, sized to the same step as the other composer controls
 * so the row reads as one set of buttons rather than a text button wedged
 * between glyphs. What used to be a labelled pill spent a third of the row's
 * width saying "Mic" next to an unmistakable microphone.
 *
 * While recording the button fills with a danger tint and a halo behind it
 * breathes with the input level — the same information the old inline meter
 * carried, in the space a button already occupies. Transcription shows a spinner
 * and ignores taps until the transcript lands; `denied` reads as a struck-through
 * mic and a tap re-triggers the permission path (which surfaces the Settings
 * hint) rather than doing nothing.
 *
 * Rendered by the composer only while connected — a phone with no desktop to
 * transcribe for it never shows this button at all.
 */
export function MicButton({ phase, level, onStart, onStop }: Props) {
  const theme = useTheme();
  const recording = phase === 'recording';
  const transcribing = phase === 'transcribing';
  const denied = phase === 'denied';

  const onPress = () => {
    if (recording) onStop();
    else if (!transcribing) onStart();
  };

  return (
    <Pressable
      onPress={onPress}
      disabled={transcribing}
      accessibilityRole="button"
      accessibilityLabel={
        recording
          ? 'Stop dictation'
          : transcribing
            ? 'Transcribing'
            : denied
              ? 'Microphone access is off'
              : 'Dictate a prompt'
      }
      accessibilityState={{ disabled: transcribing, busy: transcribing }}
      hitSlop={IconHitSlop}
      style={({ pressed }) => [
        styles.button,
        { backgroundColor: recording ? withAlpha(theme.danger, 0.16) : 'transparent' },
        pressed && styles.pressed,
      ]}
    >
      {/* The level halo sits behind the glyph and scales with what the mic is
          hearing, so silence is visible as stillness rather than as a meter
          that happens to be near zero. */}
      {recording ? (
        <View
          pointerEvents="none"
          style={[
            styles.halo,
            {
              backgroundColor: withAlpha(theme.danger, 0.28),
              transform: [{ scale: 0.55 + Math.min(1, Math.max(0, level)) * 0.45 }],
            },
          ]}
        />
      ) : null}

      {transcribing ? (
        <ActivityIndicator size="small" color={theme.textSecondary} />
      ) : (
        <Icon
          icon={recording ? Square : denied ? MicOff : Mic}
          size="lg"
          color={recording ? theme.danger : denied ? theme.textMuted : theme.textSecondary}
        />
      )}
    </Pressable>
  );
}

const styles = StyleSheet.create({
  button: {
    width: ControlHeight.compact,
    height: ControlHeight.compact,
    borderRadius: Radius.full,
    alignItems: 'center',
    justifyContent: 'center',
  },
  pressed: { opacity: 0.6 },
  halo: {
    position: 'absolute',
    top: 0,
    left: 0,
    right: 0,
    bottom: 0,
    borderRadius: Radius.full,
  },
});
