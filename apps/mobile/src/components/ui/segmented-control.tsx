import { useEffect, useState } from 'react';
import { Pressable, StyleSheet, View, type LayoutChangeEvent } from 'react-native';
import Animated, { useAnimatedStyle, useSharedValue, withSpring } from 'react-native-reanimated';

import { ThemedText } from '@/components/themed-text';
import { Radius, Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { tick } from '@/native/haptics';

/** Matches the sheet's open feel so motion reads as one system, not per-component. */
const SPRING = { damping: 22, stiffness: 240 } as const;
const PAD = 3;

export type Segment<T extends string> = { value: T; label: string };

type Props<T extends string> = {
  segments: Segment<T>[];
  value: T;
  onChange: (value: T) => void;
  accessibilityLabel?: string;
};

/**
 * A segmented choice with a highlight that *slides* between options instead of the
 * instant background-swap the screens hand-rolled. The thumb is a single absolute
 * layer translated to the selected index; the labels sit above it. Width is
 * measured once so the thumb can size itself to an equal share, which keeps the
 * component agnostic to how many segments it is given.
 */
export function SegmentedControl<T extends string>({
  segments,
  value,
  onChange,
  accessibilityLabel,
}: Props<T>) {
  const theme = useTheme();
  const [width, setWidth] = useState(0);
  const selectedIndex = Math.max(
    0,
    segments.findIndex((s) => s.value === value)
  );
  const pos = useSharedValue(selectedIndex);

  useEffect(() => {
    // eslint-disable-next-line react-hooks/immutability
    pos.value = withSpring(selectedIndex, SPRING);
  }, [selectedIndex, pos]);

  const segWidth = width > 0 ? (width - PAD * 2) / segments.length : 0;
  const thumbStyle = useAnimatedStyle(() => ({
    transform: [{ translateX: pos.value * segWidth }],
  }));

  const onLayout = (e: LayoutChangeEvent) => setWidth(e.nativeEvent.layout.width);

  return (
    <View
      onLayout={onLayout}
      accessibilityRole="tablist"
      accessibilityLabel={accessibilityLabel}
      style={[styles.track, { backgroundColor: theme.surface1, borderColor: theme.border }]}
    >
      {segWidth > 0 ? (
        <Animated.View
          style={[styles.thumb, { width: segWidth, backgroundColor: theme.backgroundSelected }, thumbStyle]}
        />
      ) : null}
      {segments.map((segment) => {
        const selected = segment.value === value;
        return (
          <Pressable
            key={segment.value}
            onPress={() => {
              if (!selected) tick();
              onChange(segment.value);
            }}
            accessibilityRole="tab"
            accessibilityState={{ selected }}
            style={styles.segment}
          >
            <ThemedText type="code" themeColor={selected ? 'text' : 'textMuted'}>
              {segment.label}
            </ThemedText>
          </Pressable>
        );
      })}
    </View>
  );
}

const styles = StyleSheet.create({
  track: {
    flexDirection: 'row',
    borderRadius: Radius.md,
    borderWidth: StyleSheet.hairlineWidth,
    padding: PAD,
    position: 'relative',
  },
  // The sliding highlight sits behind the labels; top/bottom inset by the track
  // padding so it reads as a thumb inside the groove.
  thumb: {
    position: 'absolute',
    top: PAD,
    bottom: PAD,
    left: PAD,
    borderRadius: Radius.sm,
  },
  segment: {
    flex: 1,
    alignItems: 'center',
    justifyContent: 'center',
    paddingVertical: Spacing.two,
  },
});
