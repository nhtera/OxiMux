import { useEffect } from 'react';
import { Pressable, StyleSheet } from 'react-native';
import Animated, {
  interpolate,
  interpolateColor,
  useAnimatedStyle,
  useSharedValue,
  withTiming,
} from 'react-native-reanimated';

import { Radius } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { tick } from '@/native/haptics';

// Track/thumb geometry derived once: the thumb inset by PAD on all sides, and its
// travel is whatever horizontal room is left. Changing the track size reflows the
// thumb automatically rather than needing a second hand-tuned number.
const TRACK_W = 46;
const TRACK_H = 28;
const PAD = 2;
const THUMB = TRACK_H - PAD * 2;
const TRAVEL = TRACK_W - THUMB - PAD * 2;
const DURATION = 180;

type Props = {
  value: boolean;
  onValueChange: (value: boolean) => void;
  disabled?: boolean;
  accessibilityLabel?: string;
};

/**
 * A token-driven toggle. The OS `Switch` renders its own platform green/blue and
 * ignores the app accent; this one animates the track `surface3 → accent` and
 * slides the thumb, so a toggle looks like the rest of the app in both schemes.
 * Props mirror RN `Switch` (`value`/`onValueChange`) so call sites swap 1:1.
 */
export function Switch({ value, onValueChange, disabled = false, accessibilityLabel }: Props) {
  const theme = useTheme();
  const progress = useSharedValue(value ? 1 : 0);

  useEffect(() => {
    // eslint-disable-next-line react-hooks/immutability
    progress.value = withTiming(value ? 1 : 0, { duration: DURATION });
  }, [value, progress]);

  const trackStyle = useAnimatedStyle(() => ({
    backgroundColor: interpolateColor(progress.value, [0, 1], [theme.surface3, theme.accent]),
  }));
  const thumbStyle = useAnimatedStyle(() => ({
    transform: [{ translateX: interpolate(progress.value, [0, 1], [0, TRAVEL]) }],
  }));

  return (
    <Pressable
      accessibilityRole="switch"
      accessibilityState={{ checked: value, disabled }}
      accessibilityLabel={accessibilityLabel}
      disabled={disabled}
      onPress={() => {
        tick();
        onValueChange(!value);
      }}
      style={disabled ? styles.disabled : undefined}
    >
      <Animated.View style={[styles.track, trackStyle]}>
        <Animated.View style={[styles.thumb, { backgroundColor: theme.accentText }, thumbStyle]} />
      </Animated.View>
    </Pressable>
  );
}

const styles = StyleSheet.create({
  track: {
    width: TRACK_W,
    height: TRACK_H,
    borderRadius: Radius.full,
    padding: PAD,
    justifyContent: 'center',
  },
  thumb: { width: THUMB, height: THUMB, borderRadius: Radius.full },
  disabled: { opacity: 0.5 },
});
