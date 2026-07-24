import { useCallback, useEffect, useRef, useState } from 'react';
import {
  ScrollView,
  StyleSheet,
  TextInput,
  View,
  useWindowDimensions,
  type LayoutChangeEvent,
  type StyleProp,
  type ViewStyle,
} from 'react-native';
import { Gesture, GestureDetector } from 'react-native-gesture-handler';
import { useReanimatedKeyboardAnimation } from 'react-native-keyboard-controller';
import Animated, {
  runOnJS,
  useAnimatedStyle,
  useSharedValue,
  withSpring,
  withTiming,
} from 'react-native-reanimated';
import { useSafeAreaInsets } from 'react-native-safe-area-context';

import { Radius, Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';

/** Re-exported for call-site parity with the previous sheet. The drag-to-dismiss
 *  pan is scoped to the grab handle (not the whole sheet), so a plain RN input no
 *  longer fights the sheet gesture — hence this is just `TextInput`. */
export const BottomSheetTextInput = TextInput;

type Props = {
  visible: boolean;
  onClose: () => void;
  children: React.ReactNode;
  contentStyle?: StyleProp<ViewStyle>;
};

const OPEN_SPRING = { damping: 22, stiffness: 240 } as const;
const CLOSE_MS = 200;
/** Commit dismiss past a third of the sheet's height, or on a fast downward flick. */
const DISMISS_VELOCITY = 600;

/**
 * The one bottom sheet — a hand-rolled reanimated sheet behind a controlled
 * `visible`/`onClose` API, matching the call sites the old `Modal` sheets used. A
 * single `progress` value (0 closed → 1 open) drives the slide and the backdrop
 * dim together so a drag never desyncs them; the sheet measures its own content
 * and translates by that height, and rises with the keyboard. Content taller than
 * the screen scrolls internally. Modeled on the verified swipe-deck drawer, so it
 * runs on bare reanimated + gesture-handler under the root `GestureHandlerRootView`
 * (no portal provider, no `Modal`).
 */
export function Sheet({ visible, onClose, children, contentStyle }: Props) {
  const theme = useTheme();
  const insets = useSafeAreaInsets();
  const { height: screenH } = useWindowDimensions();
  const keyboard = useReanimatedKeyboardAnimation();

  // Mount spans the open+close animation; the sheet unmounts only after the close
  // settles, so the slide-out is never cut off by an early teardown.
  const [mounted, setMounted] = useState(visible);
  const progress = useSharedValue(0);
  // Seed with the screen height so the first frame sits safely offscreen-down
  // until onLayout measures the real content height.
  const sheetH = useSharedValue(screenH);
  const opened = useRef(false);

  useEffect(() => {
    if (visible) {
      // An animated mount must keep the sheet in the tree to play its slide-out; a
      // render-derived flag cannot defer the unmount past the close animation.
      // eslint-disable-next-line react-hooks/set-state-in-effect
      setMounted(true);
    } else {
      opened.current = false;
      progress.value = withTiming(0, { duration: CLOSE_MS }, (finished) => {
        if (finished) runOnJS(setMounted)(false);
      });
    }
  }, [visible, progress]);

  // Open once the sheet is mounted and its height is known, so the slide starts
  // from a correct offscreen position rather than jumping mid-animation.
  const onLayout = useCallback(
    (e: LayoutChangeEvent) => {
      const h = e.nativeEvent.layout.height;
      if (h <= 0) return;
      // eslint-disable-next-line react-hooks/immutability
      sheetH.value = h;
      if (visible && !opened.current) {
        opened.current = true;
        // eslint-disable-next-line react-hooks/immutability
        progress.value = withSpring(1, OPEN_SPRING);
      }
    },
    [visible, progress, sheetH]
  );

  // Drag lives on the grab handle only, so it never fights the inner ScrollView
  // or a focused text field.
  const pan = Gesture.Pan()
    .onUpdate((e) => {
      'worklet';
      // Dragging up past the open position is a no-op; only downward dismisses.
      const next = 1 - Math.max(0, e.translationY) / sheetH.value;
      // eslint-disable-next-line react-hooks/immutability
      progress.value = Math.max(0, next);
    })
    .onEnd((e) => {
      'worklet';
      const dismiss = e.translationY > sheetH.value / 3 || e.velocityY > DISMISS_VELOCITY;
      if (dismiss) {
        runOnJS(onClose)();
      } else {
        // eslint-disable-next-line react-hooks/immutability
        progress.value = withSpring(1, OPEN_SPRING);
      }
    });

  const backdropStyle = useAnimatedStyle(() => ({ opacity: progress.value * 0.5 }));
  const sheetStyle = useAnimatedStyle(() => ({
    transform: [{ translateY: (1 - progress.value) * sheetH.value + keyboard.height.value }],
  }));

  if (!mounted) return null;

  return (
    <View style={StyleSheet.absoluteFill} pointerEvents="box-none">
      {/* A plain black scrim reads the same in light and dark, so it stays a fixed
          colour rather than a theme token. */}
      <Animated.View style={[styles.backdrop, backdropStyle]} onTouchEnd={onClose} />
      <Animated.View
        onLayout={onLayout}
        style={[
          styles.sheet,
          { backgroundColor: theme.surface1, maxHeight: screenH - insets.top - Spacing.six },
          sheetStyle,
        ]}
      >
        <GestureDetector gesture={pan}>
          <View style={styles.grabArea}>
            <View style={[styles.grabber, { backgroundColor: theme.borderStrong }]} />
          </View>
        </GestureDetector>
        <ScrollView
          keyboardShouldPersistTaps="handled"
          contentContainerStyle={[
            styles.content,
            { paddingBottom: insets.bottom + Spacing.four },
            contentStyle,
          ]}
        >
          {children}
        </ScrollView>
      </Animated.View>
    </View>
  );
}

const styles = StyleSheet.create({
  backdrop: { position: 'absolute', top: 0, left: 0, right: 0, bottom: 0, backgroundColor: '#000000', zIndex: 1 },
  sheet: {
    position: 'absolute',
    left: 0,
    right: 0,
    bottom: 0,
    borderTopLeftRadius: Radius.xl,
    borderTopRightRadius: Radius.xl,
    zIndex: 2,
  },
  grabArea: { alignItems: 'center', paddingVertical: Spacing.two },
  grabber: { width: 36, height: 4, borderRadius: 2 },
  content: {
    paddingHorizontal: Spacing.four,
    paddingTop: Spacing.one,
    gap: Spacing.two,
  },
});
