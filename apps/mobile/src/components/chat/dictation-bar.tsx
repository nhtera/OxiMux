import { ArrowUp, Square, X } from 'lucide-react-native';
import { Pressable, StyleSheet, View } from 'react-native';

import { ThinkingShimmer } from '@/components/chat/thinking-shimmer';
import { ControlHeight, IconHitSlop } from '@/components/ui/control-geometry';
import { Icon } from '@/components/ui/icon';
import { Radius, Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { WAVEFORM_SAMPLES, type DictationPhase } from '@/native/use-dictation';

/** Bar geometry. Narrow bars with a narrow gap read as a trace rather than as a
 * bar chart, which is what makes a run of them look like sound. */
/** Marks one bar of the trace. The heights are the only proof that the meter is
 * tracking the microphone, and a screenshot of a quiet room cannot show it. */
export const SAMPLE_TEST_ID = 'dictation-level-sample';

const BAR_WIDTH = 3;
const BAR_GAP = 3;
const BAR_MIN = 3;
const BAR_MAX = 22;

type Props = {
  phase: Extract<DictationPhase, 'recording' | 'transcribing'>;
  /** Recent input levels, oldest first, each `0..1`. */
  levels: number[];
  /** Discard the clip and return to the composer. */
  onCancel: () => void;
  /** End the recording and put the transcript in the composer. */
  onStop: () => void;
  /** End the recording and send the transcript as a prompt. */
  onSend: () => void;
};

/**
 * The composer while dictating.
 *
 * Recording takes over the whole dock rather than lighting up one button in a row
 * of five. Speaking is a *mode* — the text field, the attach button and the
 * model chips are all unreachable during it — and a bar that keeps drawing them
 * invites taps that cannot land. What replaces them is the one thing worth
 * showing (that the microphone is hearing something) and the three ways out.
 *
 * The three are ordered by how destructive they are, left to right: discard, keep
 * as text, send. Keeping and sending are separated because dictation is where the
 * two intents genuinely part company — reading a transcript back before it goes
 * out is the careful path, and never touching the keyboard is the whole point of
 * the hands-free one.
 */
export function DictationBar({ phase, levels, onCancel, onStop, onSend }: Props) {
  const theme = useTheme();
  const transcribing = phase === 'transcribing';

  return (
    <View style={styles.row}>
      <Pressable
        onPress={onCancel}
        hitSlop={IconHitSlop}
        accessibilityRole="button"
        accessibilityLabel="Discard this dictation"
        style={({ pressed }) => [
          styles.circle,
          { backgroundColor: theme.surface2 },
          pressed && styles.pressed,
        ]}
      >
        <Icon icon={X} size="lg" color={theme.textSecondary} />
      </Pressable>

      {transcribing ? (
        // The clip is already on its way to the desktop, so there is no level
        // left to draw. The shimmer is the app's existing "working" vocabulary,
        // which beats a second spinner next to the one in the stop slot.
        <View style={styles.status}>
          <ThinkingShimmer label="Transcribing" type="small" />
        </View>
      ) : (
        <Waveform levels={levels} />
      )}

      <Pressable
        onPress={onStop}
        disabled={transcribing}
        hitSlop={IconHitSlop}
        accessibilityRole="button"
        accessibilityLabel="Stop dictating and keep the text"
        accessibilityState={{ disabled: transcribing }}
        style={({ pressed }) => [
          styles.circle,
          { backgroundColor: theme.surface2 },
          pressed && styles.pressed,
          transcribing && styles.disabled,
        ]}
      >
        <Icon icon={Square} size="md" color={theme.text} />
      </Pressable>

      {/* Filled like the composer's own send button, because it is the same
          action — the transcript just takes the place of what was typed. */}
      <Pressable
        onPress={onSend}
        disabled={transcribing}
        accessibilityRole="button"
        accessibilityLabel="Stop dictating and send"
        accessibilityState={{ disabled: transcribing }}
        style={({ pressed }) => [
          styles.circle,
          { backgroundColor: theme.accent },
          pressed && styles.pressed,
          transcribing && styles.disabled,
        ]}
      >
        <Icon icon={ArrowUp} size="lg" color={theme.accentText} />
      </Pressable>
    </View>
  );
}

/**
 * The level trace: newest sample at the right, older ones sliding off the left.
 *
 * Right-aligned inside a clipping row rather than measured and fitted, so the
 * trace fills whatever width is left over and simply loses its oldest bars on a
 * narrow screen — the same way it loses them to age. Samples arrive already
 * capped in number, and any that have not arrived yet are drawn as resting dots,
 * so the bar looks like an instrument at rest instead of a half-empty container.
 */
function Waveform({ levels }: { levels: number[] }) {
  const theme = useTheme();
  // Pad the not-yet-recorded past so the trace starts full-width and *travels*
  // into view, rather than growing from the right edge for its first four seconds.
  const bars =
    levels.length >= WAVEFORM_SAMPLES
      ? levels
      : [...(new Array(WAVEFORM_SAMPLES - levels.length).fill(0) as number[]), ...levels];

  return (
    <View style={styles.wave} pointerEvents="none">
      {bars.map((level, i) => {
        const clamped = Math.min(1, Math.max(0, level));
        return (
          <View
            key={i}
            testID={SAMPLE_TEST_ID}
            style={[
              styles.bar,
              {
                height: BAR_MIN + clamped * (BAR_MAX - BAR_MIN),
                // Resting samples fade back so the live part of the trace is
                // what the eye follows.
                backgroundColor: clamped > 0.02 ? theme.textSecondary : theme.textMuted,
              },
            ]}
          />
        );
      })}
    </View>
  );
}

const styles = StyleSheet.create({
  row: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: Spacing.two,
    // Matches the resting height of the input it stands in for, so entering and
    // leaving dictation does not resize the dock.
    minHeight: ControlHeight.field,
  },
  status: { flex: 1, alignItems: 'center', justifyContent: 'center' },
  wave: {
    flex: 1,
    flexDirection: 'row',
    alignItems: 'center',
    justifyContent: 'flex-end',
    gap: BAR_GAP,
    height: BAR_MAX,
    overflow: 'hidden',
  },
  bar: { width: BAR_WIDTH, borderRadius: Radius.full },
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
