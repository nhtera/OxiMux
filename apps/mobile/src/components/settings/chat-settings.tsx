import { Pressable, StyleSheet, Switch, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { useChatPreferences, type ToolDetail } from '@/stores/chat-preferences';

const DETAIL_OPTIONS: { value: ToolDetail; label: string }[] = [
  { value: 'overview', label: 'Overview' },
  { value: 'detailed', label: 'Detailed' },
];

/**
 * Transcript display preferences: how much of each tool call to show by default,
 * and whether thinking blocks start open. Both persist via the chat-preferences
 * store, so the transcript renders the user's choice on the next snapshot.
 */
export function ChatSettings() {
  const theme = useTheme();
  const toolDetail = useChatPreferences((s) => s.toolDetail);
  const setToolDetail = useChatPreferences((s) => s.setToolDetail);
  const autoExpandThinking = useChatPreferences((s) => s.autoExpandThinking);
  const setAutoExpandThinking = useChatPreferences((s) => s.setAutoExpandThinking);

  return (
    <View style={styles.section}>
      <ThemedText type="smallBold">Chat</ThemedText>

      <View style={styles.field}>
        <ThemedText type="small" style={styles.muted}>
          Tool call detail
        </ThemedText>
        <View style={styles.segments}>
          {DETAIL_OPTIONS.map((option) => {
            const selected = option.value === toolDetail;
            return (
              <Pressable
                key={option.value}
                onPress={() => setToolDetail(option.value)}
                accessibilityRole="radio"
                accessibilityState={{ selected }}
                style={[
                  styles.segment,
                  { borderColor: theme.backgroundSelected },
                  selected && { backgroundColor: theme.backgroundSelected },
                ]}
              >
                <ThemedText type="code" style={!selected && styles.muted}>
                  {option.label}
                </ThemedText>
              </Pressable>
            );
          })}
        </View>
      </View>

      <View style={styles.toggleRow}>
        <ThemedText type="small" style={styles.muted}>
          Expand thinking by default
        </ThemedText>
        <Switch
          value={autoExpandThinking}
          onValueChange={setAutoExpandThinking}
          trackColor={{ true: theme.accent }}
          accessibilityLabel="Expand thinking by default"
        />
      </View>
    </View>
  );
}

const styles = StyleSheet.create({
  section: { gap: Spacing.two },
  field: { gap: Spacing.one },
  muted: { opacity: 0.7 },
  segments: { flexDirection: 'row', gap: Spacing.two },
  segment: {
    flex: 1,
    alignItems: 'center',
    paddingVertical: Spacing.two,
    borderRadius: Spacing.two,
    borderWidth: StyleSheet.hairlineWidth,
  },
  toggleRow: { flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between' },
});
