import { useState } from 'react';
import { Image } from 'expo-image';
import { ChevronDown, ChevronRight, TriangleAlert } from 'lucide-react-native';
import { Pressable, ScrollView, StyleSheet, View } from 'react-native';

import { ImageLightbox, dataUri } from '@/components/chat/image-lightbox';
import { SubagentLogPanel } from '@/components/chat/subagent-log-panel';
import { ThinkingShimmer } from '@/components/chat/thinking-shimmer';
import { toolIcon } from '@/components/chat/tool-icon';
import { ThemedText } from '@/components/themed-text';
import { Icon } from '@/components/ui/icon';
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
  const completed = call.status === 'Completed';
  // A finished, successful call needs no trailing word — its name and (on tap) its
  // body say enough. Everything else (failed, rejected, pending, needs-input)
  // shows the status so the state is legible without expanding.
  const showStatus = !running && !completed;
  const result = call.redact_result ? '[redacted]' : (call.result ?? '');
  const lines = result.split('\n');
  // A long result gets a bounded, internally-scrolling frame so it never pushes
  // the rest of the transcript around.
  const bounded = lines.length > RESULT_PREVIEW_LINES * 3;

  return (
    <View
      style={[
        styles.tool,
        { backgroundColor: theme.backgroundElement, borderColor: expanded ? theme.border : 'transparent' },
      ]}
    >
      {/* Collapsed, the card is a pill: icon · name · status · chevron. The border
          only appears once expanded, so a run of calls reads as a quiet stack. */}
      <Pressable
        onPress={() => setExpanded((v) => !v)}
        style={styles.toolHeader}
        accessibilityRole="button"
        accessibilityState={{ expanded }}
      >
        <Icon
          icon={failed ? TriangleAlert : toolIcon(call.name)}
          size="sm"
          color={failed ? theme.danger : theme.textSecondary}
        />
        <View style={styles.nameWrap}>
          {running ? (
            // The name itself shimmers while the call runs — the "this is working"
            // signal rides on the label rather than a separate caption.
            <ThinkingShimmer label={call.name} type="code" numberOfLines={1} />
          ) : (
            <ThemedText type="code" numberOfLines={1}>
              {call.name}
            </ThemedText>
          )}
        </View>
        {showStatus ? (
          <ThemedText type="small" themeColor={failed ? 'danger' : 'textMuted'}>
            {statusLabel(call.status)}
          </ThemedText>
        ) : null}
        <Icon icon={expanded ? ChevronDown : ChevronRight} size="sm" color={theme.textMuted} />
      </Pressable>

      {expanded ? (
        <>
          {failed ? (
            <ThemedText type="small" themeColor="danger">
              {failed}
            </ThemedText>
          ) : null}

          {call.terminal_id ? (
            <ThemedText type="small" themeColor="textMuted">
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
                {result}
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
        </>
      ) : null}

      {/* The approval card stays visible even collapsed — a call blocked on a
          permission must not hide its prompt behind a tap. */}
      {children}

      <ImageLightbox image={zoom} onClose={() => setZoom(null)} />
    </View>
  );
}

const styles = StyleSheet.create({
  // A hairline border is always present but transparent when collapsed, so the
  // border appearing on expand costs no layout shift.
  tool: {
    borderRadius: Radius.md,
    borderWidth: StyleSheet.hairlineWidth,
    paddingVertical: Spacing.two,
    paddingHorizontal: Spacing.three,
    gap: Spacing.two,
  },
  toolHeader: { flexDirection: 'row', alignItems: 'center', gap: Spacing.two },
  nameWrap: { flex: 1 },
  result: { opacity: 0.85, lineHeight: 16 },
  bounded: { maxHeight: MAX_RESULT_HEIGHT },
  thumbs: { flexDirection: 'row', flexWrap: 'wrap', gap: Spacing.two },
  thumb: { width: THUMB, height: THUMB, borderRadius: Radius.md },
});
