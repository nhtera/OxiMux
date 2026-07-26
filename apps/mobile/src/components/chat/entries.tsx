import { useState } from 'react';
import { Pressable, StyleSheet, View } from 'react-native';

import { AttachmentThumb } from '@/components/chat/attachment-thumb';
import { ImageLightbox } from '@/components/chat/image-lightbox';
import { MarkdownBody } from '@/components/chat/markdown-body';
import { MessageActions } from '@/components/chat/message-actions';
import { ThemedText } from '@/components/themed-text';
import { Radius, Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { useChatPreferences } from '@/stores/chat-preferences';
import { type AssistantMessage, type ChatImage } from '@/native/thread';

export function UserBubble({ text, images }: { text: string; images: ChatImage[] }) {
  const theme = useTheme();
  const [zoom, setZoom] = useState<ChatImage | null>(null);
  return (
    <View style={styles.userRow}>
      <View style={styles.userCol}>
        {/* Above the bubble rather than inside it, matching the desktop: an
            attachment is its own thing, and nesting it would make the bubble
            stretch to the image's width and swallow the tail corner. */}
        {images.length > 0 ? (
          <View style={styles.thumbs}>
            {images.map((image, i) => (
              <AttachmentThumb
                key={i}
                image={image}
                style={styles.thumb}
                label="View attached image"
                onPress={() => setZoom(image)}
              />
            ))}
          </View>
        ) : null}
        {/* An image-only prompt renders no bubble at all — an empty rounded box
            beneath the thumbnail reads as a rendering fault. */}
        {text ? (
          <View style={[styles.userBubble, { backgroundColor: theme.backgroundSelected }]}>
            <ThemedText style={styles.body}>{text}</ThemedText>
          </View>
        ) : null}
        {text ? <MessageActions text={text} /> : null}
        <ImageLightbox image={zoom} onClose={() => setZoom(null)} />
      </View>
    </View>
  );
}

export function AssistantBubble({ message }: { message: AssistantMessage }) {
  const autoExpand = useChatPreferences((s) => s.autoExpandThinking);
  const [showThinking, setShowThinking] = useState(autoExpand);
  const hasThinking = message.thinking.trim().length > 0;

  return (
    <View style={styles.assistant}>
      {hasThinking ? (
        <Pressable onPress={() => setShowThinking((v) => !v)} hitSlop={Spacing.two}>
          <ThemedText type="small" themeColor="textMuted">
            {showThinking ? '▾ Thinking' : '▸ Thinking'}
          </ThemedText>
        </Pressable>
      ) : null}
      {hasThinking && showThinking ? (
        <ThemedText type="small" themeColor="textMuted" style={styles.thinking}>
          {message.thinking}
        </ThemedText>
      ) : null}
      {message.text ? <MarkdownBody text={message.text} /> : null}
      {message.text ? <MessageActions text={message.text} /> : null}
    </View>
  );
}

export function CompactionDivider({ summary }: { summary: string }) {
  return (
    <View style={styles.divider}>
      <ThemedText type="small" themeColor="textMuted">
        {summary || 'Earlier context was compacted.'}
      </ThemedText>
    </View>
  );
}

// `ToolCallCard` moved to ./tool-call-card.tsx when it gained image thumbnails
// and detail levels; `TurnDiffCard` to ./turn-diff-card.tsx for its expandable
// hunks. Both re-exported so the transcript's imports do not have to care.
export { ToolCallCard } from '@/components/chat/tool-call-card';
export { TurnDiffCard } from '@/components/chat/turn-diff-card';

const styles = StyleSheet.create({
  body: { lineHeight: 22 },
  userRow: { flexDirection: 'row', justifyContent: 'flex-end' },
  userCol: { maxWidth: '88%', alignItems: 'flex-end', gap: Spacing.one },
  // The sharp top-right corner is the chat-bubble "tail" that marks this as an
  // outgoing message; the other three stay fully rounded.
  userBubble: {
    borderRadius: Radius.lg,
    borderTopRightRadius: Radius.sm,
    paddingHorizontal: Spacing.three,
    paddingVertical: Spacing.two,
    gap: Spacing.one,
  },
  // Larger than the tool-call card's thumbnail: that one is an incidental
  // result among many rows, while this is the thing the user chose to send and
  // wants to recognise at a glance without opening the viewer.
  thumbs: { flexDirection: 'row', flexWrap: 'wrap', justifyContent: 'flex-end', gap: Spacing.one },
  thumb: { width: 128, height: 128, borderRadius: Radius.md },
  assistant: { gap: Spacing.one },
  thinking: { fontStyle: 'italic' },
  divider: { alignItems: 'center', paddingVertical: Spacing.two },
});
