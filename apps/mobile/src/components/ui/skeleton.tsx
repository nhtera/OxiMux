import { useEffect } from 'react';
import { StyleSheet, type StyleProp, View, type ViewStyle } from 'react-native';
import Animated, { useAnimatedStyle, useSharedValue, withRepeat, withTiming } from 'react-native-reanimated';

import { Radius } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';

/**
 * A single pulsing placeholder block. Compose these into a shape that matches the
 * real row (title bar, subtitle bar, trailing dot) so a first load reads as
 * "content arriving here", not a generic spinner floating in space.
 */
export function Skeleton({
  width,
  height = 14,
  radius = Radius.sm,
  style,
}: {
  width: number | `${number}%`;
  height?: number;
  radius?: number;
  style?: StyleProp<ViewStyle>;
}) {
  const theme = useTheme();
  const pulse = useSharedValue(0.4);

  useEffect(() => {
    pulse.value = withRepeat(withTiming(0.8, { duration: 900 }), -1, true);
    return () => {};
  }, [pulse]);

  const animatedStyle = useAnimatedStyle(() => ({ opacity: pulse.value }));

  return (
    <Animated.View
      style={[
        { width, height, borderRadius: radius, backgroundColor: theme.surface2 },
        animatedStyle,
        style,
      ]}
    />
  );
}

/** A stack of skeleton rows shaped like a list of title + subtitle items. */
export function SkeletonList({ rows = 5 }: { rows?: number }) {
  return (
    <View style={styles.list}>
      {Array.from({ length: rows }).map((_, i) => (
        <View key={i} style={styles.row}>
          <View style={styles.rowText}>
            <Skeleton width="70%" height={16} />
            <Skeleton width="40%" height={12} />
          </View>
        </View>
      ))}
    </View>
  );
}

const styles = StyleSheet.create({
  list: { padding: 16, gap: 16 },
  row: { flexDirection: 'row', alignItems: 'center', gap: 12 },
  rowText: { flex: 1, gap: 8 },
});
