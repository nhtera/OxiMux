import { StyleSheet, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { SegmentedControl } from '@/components/ui/segmented-control';
import { Switch } from '@/components/ui/switch';
import { Spacing } from '@/constants/theme';
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
  const toolDetail = useChatPreferences((s) => s.toolDetail);
  const setToolDetail = useChatPreferences((s) => s.setToolDetail);
  const autoExpandThinking = useChatPreferences((s) => s.autoExpandThinking);
  const setAutoExpandThinking = useChatPreferences((s) => s.setAutoExpandThinking);

  return (
    <View style={styles.section}>
      <ThemedText type="smallBold">Chat</ThemedText>

      <View style={styles.field}>
        <ThemedText type="small" themeColor="textMuted">
          Tool call detail
        </ThemedText>
        <SegmentedControl
          segments={DETAIL_OPTIONS}
          value={toolDetail}
          onChange={setToolDetail}
          accessibilityLabel="Tool call detail"
        />
      </View>

      <View style={styles.toggleRow}>
        <ThemedText type="small" themeColor="textMuted">
          Expand thinking by default
        </ThemedText>
        <Switch
          value={autoExpandThinking}
          onValueChange={setAutoExpandThinking}
          accessibilityLabel="Expand thinking by default"
        />
      </View>
    </View>
  );
}

const styles = StyleSheet.create({
  section: { gap: Spacing.two },
  field: { gap: Spacing.one },
  toggleRow: { flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between' },
});
