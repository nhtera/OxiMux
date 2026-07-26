import { Image } from 'expo-image';
import { ImageOff } from 'lucide-react-native';
import { Pressable, StyleSheet, View } from 'react-native';

import { dataUri, isElided } from '@/components/chat/image-lightbox';
import { ThemedText } from '@/components/themed-text';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import type { ChatImage } from '@/native/thread';

/**
 * One attached or returned image, at whatever size the caller's `style` sets.
 *
 * Shared between the user bubble and the tool-call card because the interesting
 * part is not the layout — those two deliberately differ in size — but the
 * decision about what to draw when there are no pixels to draw, which has to be
 * the same in both places or the transcript contradicts itself.
 *
 * An image whose payload was dropped in transit renders as a labelled tile and is
 * **not** pressable: opening a viewer onto nothing is a dead end, and the label has
 * already said everything there is to say about it.
 */
/**
 * The caller's chosen tile geometry.
 *
 * Spelled out rather than taken as a `StyleProp`, because the two branches below
 * are a `View` and an `expo-image` and their style types are not interchangeable
 * — `overflow: 'scroll'` is meaningful to one and not the other. Naming the three
 * properties a thumbnail actually varies by satisfies both without a cast, and
 * says plainly that this is a size, not a free hand over the appearance.
 */
export type ThumbSize = { width: number; height: number; borderRadius: number };

export function AttachmentThumb({
  image,
  style,
  label,
  onPress,
}: {
  image: ChatImage;
  style: ThumbSize;
  /** Accessibility label for the openable image. */
  label: string;
  onPress: () => void;
}) {
  const theme = useTheme();

  if (isElided(image)) {
    return (
      <View
        style={[
          style,
          styles.elided,
          { backgroundColor: theme.surface2, borderColor: theme.border },
        ]}
        accessibilityRole="image"
        accessibilityLabel="Image not transferred"
      >
        <ImageOff size={18} color={theme.textMuted} />
        <ThemedText style={[styles.elidedText, { color: theme.textMuted }]} numberOfLines={2}>
          Not transferred
        </ThemedText>
      </View>
    );
  }

  return (
    <Pressable onPress={onPress} accessibilityRole="imagebutton" accessibilityLabel={label}>
      <Image
        source={{ uri: dataUri(image) }}
        style={[style, { backgroundColor: theme.surface2 }]}
        contentFit="cover"
      />
    </Pressable>
  );
}

const styles = StyleSheet.create({
  elided: {
    alignItems: 'center',
    justifyContent: 'center',
    gap: Spacing.one,
    borderWidth: StyleSheet.hairlineWidth,
  },
  elidedText: { fontSize: 11, textAlign: 'center' },
});
