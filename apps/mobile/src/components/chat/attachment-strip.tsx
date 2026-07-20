import { Image } from 'expo-image';
import { Pressable, ScrollView, StyleSheet, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import type { Attachment } from '@/native/attachments';

type Props = {
  attachments: Attachment[];
  onRemove: (id: string) => void;
};

/**
 * The queued attachments, shown above the input until the prompt is sent.
 *
 * Horizontally scrolling rather than wrapping: the strip sits directly above the
 * keyboard, so growing it to a second row would push the input off screen at the
 * moment the user is typing into it.
 */
export function AttachmentStrip({ attachments, onRemove }: Props) {
  const theme = useTheme();
  if (attachments.length === 0) return null;

  return (
    <ScrollView
      horizontal
      showsHorizontalScrollIndicator={false}
      contentContainerStyle={styles.row}
      // Without this the strip stretches to the full height of the composer and
      // the thumbnails float in the middle of the empty space.
      style={styles.strip}
    >
      {attachments.map((attachment) => (
        <View key={attachment.id}>
          <Image
            source={{ uri: attachment.uri }}
            style={[styles.thumb, { backgroundColor: theme.backgroundElement }]}
            contentFit="cover"
            // A thumbnail is decorative here; the file name would say nothing
            // useful, so describe the affordance the user actually has.
            accessibilityLabel="Attached image"
          />
          <Pressable
            onPress={() => onRemove(attachment.id)}
            accessibilityLabel="Remove attached image"
            // The tap target is the badge plus its padding — a bare 16pt glyph
            // is below the minimum comfortable touch size.
            hitSlop={Spacing.two}
            style={[styles.remove, { backgroundColor: theme.background }]}
          >
            <ThemedText type="code">✕</ThemedText>
          </Pressable>
        </View>
      ))}
    </ScrollView>
  );
}

const THUMB = 56;

const styles = StyleSheet.create({
  strip: { flexGrow: 0 },
  row: { gap: Spacing.two, paddingRight: Spacing.two },
  thumb: { width: THUMB, height: THUMB, borderRadius: Spacing.two },
  remove: {
    position: 'absolute',
    top: -Spacing.one,
    right: -Spacing.one,
    width: 20,
    height: 20,
    borderRadius: 10,
    alignItems: 'center',
    justifyContent: 'center',
  },
});
