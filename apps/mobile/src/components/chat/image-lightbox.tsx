import { Image } from 'expo-image';
import { X } from 'lucide-react-native';
import { Modal, Pressable, StyleSheet, View, useWindowDimensions } from 'react-native';
import { Gesture, GestureDetector } from 'react-native-gesture-handler';
import Animated, {
  useAnimatedStyle,
  useSharedValue,
  withTiming,
} from 'react-native-reanimated';
import { useSafeAreaInsets } from 'react-native-safe-area-context';

import type { ChatImage } from '@/native/thread';

/** Don't let a pinch shrink below the fitted size or blow up past this. */
const MIN_SCALE = 1;
const MAX_SCALE = 4;

/** A base64 tool-returned image as a data URI `expo-image` can render. */
export function dataUri(image: ChatImage): string {
  return `data:${image.media_type};base64,${image.data}`;
}

/**
 * A full-screen viewer for one tool-returned image. Pinch to zoom, drag to pan
 * while zoomed, double-tap to toggle fit↔2×; the backdrop and close button both
 * dismiss. Rendered in a `Modal` so it floats above the transcript's own
 * scrolling rather than living inside it.
 */
export function ImageLightbox({ image, onClose }: { image: ChatImage | null; onClose: () => void }) {
  const insets = useSafeAreaInsets();
  const { width, height } = useWindowDimensions();

  const scale = useSharedValue(1);
  const savedScale = useSharedValue(1);
  const tx = useSharedValue(0);
  const ty = useSharedValue(0);
  const savedTx = useSharedValue(0);
  const savedTy = useSharedValue(0);

  const reset = () => {
    'worklet';
    scale.value = withTiming(1);
    savedScale.value = 1;
    tx.value = withTiming(0);
    ty.value = withTiming(0);
    savedTx.value = 0;
    savedTy.value = 0;
  };

  const pinch = Gesture.Pinch()
    .onUpdate((e) => {
      'worklet';
      scale.value = Math.min(MAX_SCALE, Math.max(MIN_SCALE, savedScale.value * e.scale));
    })
    .onEnd(() => {
      'worklet';
      savedScale.value = scale.value;
      if (scale.value <= MIN_SCALE) reset();
    });

  const pan = Gesture.Pan()
    .onUpdate((e) => {
      'worklet';
      // Panning is only meaningful once zoomed in; at fit scale the image fills
      // the frame and there is nothing to reveal by dragging.
      if (scale.value <= MIN_SCALE) return;
      tx.value = savedTx.value + e.translationX;
      ty.value = savedTy.value + e.translationY;
    })
    .onEnd(() => {
      'worklet';
      savedTx.value = tx.value;
      savedTy.value = ty.value;
    });

  const doubleTap = Gesture.Tap()
    .numberOfTaps(2)
    .onEnd(() => {
      'worklet';
      if (scale.value > MIN_SCALE) {
        reset();
      } else {
        scale.value = withTiming(2);
        savedScale.value = 2;
      }
    });

  const gesture = Gesture.Simultaneous(pinch, pan, doubleTap);

  const imageStyle = useAnimatedStyle(() => ({
    transform: [{ translateX: tx.value }, { translateY: ty.value }, { scale: scale.value }],
  }));

  return (
    <Modal visible={image !== null} transparent animationType="fade" onRequestClose={onClose}>
      <View style={styles.backdrop}>
        {/* Tap anywhere off the image closes. It sits under the gesture layer so a
            pinch/pan on the image is not read as a dismiss tap. */}
        <Pressable style={StyleSheet.absoluteFill} onPress={onClose} accessibilityLabel="Close image" />
        {image ? (
          <GestureDetector gesture={gesture}>
            <Animated.View style={[styles.stage, { width, height }, imageStyle]}>
              <Image
                source={{ uri: dataUri(image) }}
                style={styles.image}
                contentFit="contain"
                accessibilityLabel="Full-screen image"
              />
            </Animated.View>
          </GestureDetector>
        ) : null}
        <Pressable
          onPress={onClose}
          accessibilityRole="button"
          accessibilityLabel="Close"
          hitSlop={12}
          style={[styles.close, { top: insets.top + 8 }]}
        >
          <X size={24} color="#ffffff" strokeWidth={2} />
        </Pressable>
      </View>
    </Modal>
  );
}

const styles = StyleSheet.create({
  // A viewer is a full-bleed dark stage regardless of theme — an image reads
  // against black, and the surrounding chrome is deliberately absent here.
  backdrop: { flex: 1, backgroundColor: 'rgba(0,0,0,0.92)', alignItems: 'center', justifyContent: 'center' },
  stage: { alignItems: 'center', justifyContent: 'center' },
  image: { width: '100%', height: '100%' },
  close: {
    position: 'absolute',
    right: 16,
    width: 40,
    height: 40,
    alignItems: 'center',
    justifyContent: 'center',
    borderRadius: 20,
    backgroundColor: 'rgba(0,0,0,0.4)',
  },
});
