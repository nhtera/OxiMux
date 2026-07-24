import { useState } from 'react';
import { Image } from 'expo-image';
import { Pressable, ScrollView, StyleSheet, View } from 'react-native';

import { ImageLightbox, dataUri } from '@/components/chat/image-lightbox';
import { SubagentLogPanel } from '@/components/chat/subagent-log-panel';
import { ThinkingShimmer } from '@/components/chat/thinking-shimmer';
import { ThemedText } from '@/components/themed-text';
import { Radius, Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { useChatPreferences } from '@/stores/chat-preferences';
import { failure, statusLabel, type ChatImage, type ToolCall } from '@/native/thread';

/** How much of a tool result to show before it needs a tap (overview mode). */
const RESULT_PREVIEW_LINES = 6;
/** An expanded result taller than this scrolls inside the card rather than
 *  pushing the rest of the transcript off screen. */
const MAX_RESULT_HEIGHT = 320;
const THUMB = 72;

/**
 * A tool call, collapsed to its name + status until tapped. The result body is
 * the part that can run to thousands of lines, so it is what the tap reveals.
 * Whether it starts expanded follows the user's tool-detail preference.
 *
 * A call carrying `terminal_id` is an embedded live terminal. The desktop mounts
 * a real PTY view there; the phone cannot yet, so it says so rather than
 * rendering an empty frame that looks broken.
 */
export function ToolCallCard({ call, children }: { call: ToolCall; children?: React.ReactNode }) {
  const theme = useTheme();
  const detailed = useChatPreferences((s) => s.toolDetail === 'detailed');
  const [expanded, setExpanded] = useState(detailed);
  const [zoom, setZoom] = useState<ChatImage | null>(null);
  const failed = failure(call);
  const running = call.status === 'InProgress';
  const result = call.redact_result ? '[redacted]' : (call.result ?? '');
  const lines = result.split('\n');
  const preview = lines.slice(0, RESULT_PREVIEW_LINES).join('\n');
  const truncated = preview.length < result.length;
  // A long expanded result gets a bounded, internally-scrolling frame.
  const bounded = expanded && lines.length > RESULT_PREVIEW_LINES * 3;

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
        bounded ? (
          <ScrollView style={styles.bounded} nestedScrollEnabled showsVerticalScrollIndicator>
            <ThemedText type="code" style={styles.result}>
              {result}
            </ThemedText>
          </ScrollView>
        ) : (
          <ThemedText type="code" style={styles.result}>
            {expanded ? result : preview}
            {!expanded && truncated ? '\n…' : ''}
          </ThemedText>
        )
      ) : null}

      {call.images.length > 0 ? (
        <View style={styles.thumbs}>
          {call.images.map((image, i) => (
            <Pressable
              key={i}
              onPress={() => setZoom(image)}
              accessibilityRole="imagebutton"
              accessibilityLabel="View returned image"
            >
              <Image
                source={{ uri: dataUri(image) }}
                style={[styles.thumb, { backgroundColor: theme.surface2 }]}
                contentFit="cover"
              />
            </Pressable>
          ))}
        </View>
      ) : null}

      <SubagentLogPanel lines={call.subagent_log} />

      {/* The approval card, when this call is blocked on one. */}
      {children}

      <ImageLightbox image={zoom} onClose={() => setZoom(null)} />
    </View>
  );
}

const styles = StyleSheet.create({
  muted: { opacity: 0.7 },
  tool: { borderRadius: Spacing.two, padding: Spacing.three, gap: Spacing.two },
  toolHeader: { flexDirection: 'row', alignItems: 'center', gap: Spacing.two },
  toolName: { flex: 1 },
  result: { opacity: 0.85, lineHeight: 16 },
  bounded: { maxHeight: MAX_RESULT_HEIGHT },
  thumbs: { flexDirection: 'row', flexWrap: 'wrap', gap: Spacing.two },
  thumb: { width: THUMB, height: THUMB, borderRadius: Radius.md },
});
