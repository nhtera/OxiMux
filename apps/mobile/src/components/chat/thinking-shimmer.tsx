import MaskedView from '@react-native-masked-view/masked-view';
import { useEffect, useState } from 'react';
import { StyleSheet, type LayoutChangeEvent } from 'react-native';
import Animated, {
  cancelAnimation,
  Easing,
  useAnimatedStyle,
  useSharedValue,
  withRepeat,
  withTiming,
} from 'react-native-reanimated';
import Svg, { Defs, LinearGradient, Rect, Stop } from 'react-native-svg';

import { ThemedText, type ThemedTextProps } from '@/components/themed-text';
import { useTheme } from '@/hooks/use-theme';

/**
 * A highlight band sweeping across a label, masked to the glyph shapes — reads as
 * "working" far better than a spinner or a static word. The animation is torn
 * down on unmount (`cancelAnimation`), so a card that scrolls off burns nothing.
 *
 * `type`/`numberOfLines` let a caller sweep text in its own style (e.g. a mono
 * tool name that shimmers in place while the call runs) rather than a fixed size.
 */
export function ThinkingShimmer({
  label,
  type = 'small',
  numberOfLines,
}: {
  label: string;
  type?: ThemedTextProps['type'];
  numberOfLines?: number;
}) {
  const theme = useTheme();
  const [size, setSize] = useState({ width: 0, height: 0 });
  const shift = useSharedValue(0);

  useEffect(() => {
    shift.value = withRepeat(withTiming(1, { duration: 1100, easing: Easing.linear }), -1, false);
    return () => cancelAnimation(shift);
  }, [shift]);

  // Sweep the band from one text-width left of the label to one width right, so
  // the bright centre travels the full label and off each edge.
  const bandStyle = useAnimatedStyle(() => ({
    transform: [{ translateX: -size.width + shift.value * (size.width * 2) }],
  }));

  const onLayout = (e: LayoutChangeEvent) =>
    setSize({ width: e.nativeEvent.layout.width, height: e.nativeEvent.layout.height });

  return (
    <MaskedView
      style={size.height ? { height: size.height } : undefined}
      maskElement={
        <ThemedText type={type} numberOfLines={numberOfLines} onLayout={onLayout}>
          {label}
        </ThemedText>
      }
    >
      {/* Dim base under the glyphs, so the label stays legible between sweeps. */}
      <ThemedText type={type} numberOfLines={numberOfLines} style={{ color: theme.textMuted }}>
        {label}
      </ThemedText>
      {size.width > 0 ? (
        <Animated.View style={[StyleSheet.absoluteFill, bandStyle]}>
          <Svg width={size.width} height={size.height}>
            <Defs>
              <LinearGradient id="thinkingShimmer" x1="0" y1="0" x2="1" y2="0">
                <Stop offset="0" stopColor={theme.text} stopOpacity="0" />
                <Stop offset="0.5" stopColor={theme.text} stopOpacity="1" />
                <Stop offset="1" stopColor={theme.text} stopOpacity="0" />
              </LinearGradient>
            </Defs>
            <Rect width={size.width} height={size.height} fill="url(#thinkingShimmer)" />
          </Svg>
        </Animated.View>
      ) : null}
    </MaskedView>
  );
}
