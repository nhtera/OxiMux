import { useState } from 'react';
import { Pressable, StyleSheet, View } from 'react-native';

import { MarkdownBody } from '@/components/chat/markdown-body';
import { SubagentLogPanel } from '@/components/chat/subagent-log-panel';
import { ThinkingShimmer } from '@/components/chat/thinking-shimmer';
import { ThemedText } from '@/components/themed-text';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import {
  failure,
  statusLabel,
  type AssistantMessage,
  type ChatImage,
  type ToolCall,
  type TurnFileChange,
} from '@/native/thread';

/** How much of a tool result to show before it needs a tap. */
const RESULT_PREVIEW_LINES = 6;

export function UserBubble({ text, images }: { text: string; images: ChatImage[] }) {
  const theme = useTheme();
  return (
    <View style={styles.userRow}>
      <View style={[styles.userBubble, { backgroundColor: theme.backgroundSelected }]}>
        {text ? <ThemedText style={styles.body}>{text}</ThemedText> : null}
        {images.length > 0 ? (
          <ThemedText type="small" style={styles.muted}>
            {images.length === 1 ? '1 image attached' : `${images.length} images attached`}
          </ThemedText>
        ) : null}
      </View>
    </View>
  );
}

export function AssistantBubble({ message }: { message: AssistantMessage }) {
  const [showThinking, setShowThinking] = useState(false);
  const hasThinking = message.thinking.trim().length > 0;

  return (
    <View style={styles.assistant}>
      {hasThinking ? (
        <Pressable onPress={() => setShowThinking((v) => !v)} hitSlop={Spacing.two}>
          <ThemedText type="small" style={styles.muted}>
            {showThinking ? '▾ Thinking' : '▸ Thinking'}
          </ThemedText>
        </Pressable>
      ) : null}
      {hasThinking && showThinking ? (
        <ThemedText type="small" style={[styles.muted, styles.thinking]}>
          {message.thinking}
        </ThemedText>
      ) : null}
      {message.text ? <MarkdownBody text={message.text} /> : null}
    </View>
  );
}

/**
 * A tool call, collapsed to its name + status until tapped. The result body is
 * the part that can run to thousands of lines, so it is what the tap reveals.
 *
 * A call carrying `terminal_id` is an embedded live terminal. The desktop mounts
 * a real PTY view there; the phone cannot yet, so it says so rather than
 * rendering an empty frame that looks broken.
 */
export function ToolCallCard({ call, children }: { call: ToolCall; children?: React.ReactNode }) {
  const theme = useTheme();
  const [expanded, setExpanded] = useState(false);
  const failed = failure(call);
  const running = call.status === 'InProgress';
  const result = call.redact_result ? '[redacted]' : (call.result ?? '');
  const preview = result.split('\n').slice(0, RESULT_PREVIEW_LINES).join('\n');
  const truncated = preview.length < result.length;

  return (
    <View style={[styles.tool, { backgroundColor: theme.backgroundElement }]}>
      <Pressable onPress={() => setExpanded((v) => !v)} style={styles.toolHeader}>
        <ThemedText type="code" numberOfLines={1} style={styles.toolName}>
          {call.name}
        </ThemedText>
        {running ? (
          <ThinkingShimmer label="Working" />
        ) : (
          <ThemedText
            type="small"
            themeColor={failed ? 'danger' : undefined}
            style={!failed ? styles.muted : undefined}
          >
            {statusLabel(call.status)}
          </ThemedText>
        )}
      </Pressable>

      {failed ? (
        <ThemedText type="small" themeColor="danger">
          {failed}
        </ThemedText>
      ) : null}

      {call.terminal_id ? (
        <ThemedText type="small" style={styles.muted}>
          This step runs in a live terminal — open it on the desktop to watch or type.
        </ThemedText>
      ) : null}

      {result ? (
        <ThemedText type="code" style={styles.result}>
          {expanded ? result : preview}
          {!expanded && truncated ? '\n…' : ''}
        </ThemedText>
      ) : null}

      {call.images.length > 0 ? (
        <ThemedText type="small" style={styles.muted}>
          {call.images.length === 1 ? '1 image returned' : `${call.images.length} images returned`}
        </ThemedText>
      ) : null}

      <SubagentLogPanel lines={call.subagent_log} />

      {/* The approval card, when this call is blocked on one. */}
      {children}
    </View>
  );
}

export function CompactionDivider({ summary }: { summary: string }) {
  return (
    <View style={styles.divider}>
      <ThemedText type="small" style={styles.muted}>
        {summary || 'Earlier context was compacted.'}
      </ThemedText>
    </View>
  );
}

// `TurnDiffCard` moved to ./turn-diff-card.tsx when it gained an expandable
// hunk view — re-exported so the transcript's import does not have to care.
export { TurnDiffCard } from '@/components/chat/turn-diff-card';

const styles = StyleSheet.create({
  body: { lineHeight: 22 },
  muted: { opacity: 0.7 },
  userRow: { flexDirection: 'row', justifyContent: 'flex-end' },
  userBubble: {
    maxWidth: '88%',
    borderRadius: Spacing.three,
    paddingHorizontal: Spacing.three,
    paddingVertical: Spacing.two,
    gap: Spacing.one,
  },
  assistant: { gap: Spacing.one },
  thinking: { fontStyle: 'italic' },
  tool: { borderRadius: Spacing.two, padding: Spacing.three, gap: Spacing.two },
  toolHeader: { flexDirection: 'row', alignItems: 'center', gap: Spacing.two },
  toolName: { flex: 1 },
  result: { opacity: 0.85, lineHeight: 16 },
  divider: { alignItems: 'center', paddingVertical: Spacing.two },
});
