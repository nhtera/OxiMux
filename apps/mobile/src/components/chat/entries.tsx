import { useState } from 'react';
import { Pressable, StyleSheet, View } from 'react-native';

import { MarkdownBody } from '@/components/chat/markdown-body';
import { MessageActions } from '@/components/chat/message-actions';
import { ThemedText } from '@/components/themed-text';
import { Radius, Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { useChatPreferences } from '@/stores/chat-preferences';
import { type AssistantMessage, type ChatImage } from '@/native/thread';

export function UserBubble({ text, images }: { text: string; images: ChatImage[] }) {
  const theme = useTheme();
  return (
    <View style={styles.userRow}>
      <View style={styles.userCol}>
        <View style={[styles.userBubble, { backgroundColor: theme.backgroundSelected }]}>
          {text ? <ThemedText style={styles.body}>{text}</ThemedText> : null}
          {images.length > 0 ? (
            <ThemedText type="small" themeColor="textMuted">
              {images.length === 1 ? '1 image attached' : `${images.length} images attached`}
            </ThemedText>
          ) : null}
        </View>
        <MessageActions text={text} />
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
  assistant: { gap: Spacing.one },
  thinking: { fontStyle: 'italic' },
  divider: { alignItems: 'center', paddingVertical: Spacing.two },
});
