import { router } from 'expo-router';
import {
  CalendarClock,
  MessageSquare,
  Plus,
  Settings as SettingsIcon,
  SquareTerminal,
  type LucideIcon,
} from 'lucide-react-native';
import type { SessionSummary } from 'oximux-core';
import { useMemo, useState } from 'react';
import { Pressable, StyleSheet, View } from 'react-native';

import { Sheet, BottomSheetTextInput } from '@/components/ui/sheet';
import { EmptyState } from '@/components/ui/empty-state';
import { Icon } from '@/components/ui/icon';
import { ThemedText } from '@/components/themed-text';
import { Radius, Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { useClient } from '@/native/client';

type Action = {
  key: string;
  label: string;
  hint?: string;
  icon: LucideIcon;
  run: () => void;
};

/**
 * Fast navigation from anywhere: search across open sessions and the top-level
 * screens, pick one, jump. A bottom sheet rather than a full screen so it
 * overlays the current context and dismisses back to it.
 *
 * `New session` is delegated to the caller because the create flow (choosing a
 * working directory) already lives on the sessions screen; the palette should
 * not grow a second copy of it.
 */
export function CommandPalette({
  visible,
  onClose,
  onNewSession,
}: {
  visible: boolean;
  onClose: () => void;
  onNewSession: () => void;
}) {
  const theme = useTheme();
  const [query, setQuery] = useState('');
  const sessions = useClient((s) => s.sessions);

  const go = (run: () => void) => () => {
    onClose();
    // The close animation and a navigation both touch the navigator; letting the
    // dismiss settle first avoids a visible flicker of the old screen mid-slide.
    setTimeout(run, 60);
  };

  const actions = useMemo<Action[]>(() => {
    const nav: Action[] = [
      { key: 'new', label: 'New session', icon: Plus, run: onNewSession },
      { key: 'terminals', label: 'Terminals', icon: SquareTerminal, run: () => router.push('/terminals') },
      { key: 'schedules', label: 'Schedules', icon: CalendarClock, run: () => router.push('/schedules') },
      { key: 'settings', label: 'Settings', icon: SettingsIcon, run: () => router.push('/settings') },
    ];
    const sessionActions: Action[] = sessions.map((s: SessionSummary) => ({
      key: `session:${s.sessionId}`,
      label: s.title,
      hint: s.model ?? undefined,
      icon: MessageSquare,
      run: () => router.push({ pathname: '/session/[id]', params: { id: s.sessionId } }),
    }));
    return [...nav, ...sessionActions];
  }, [sessions, onNewSession]);

  const q = query.trim().toLowerCase();
  const filtered = q
    ? actions.filter((a) => a.label.toLowerCase().includes(q) || a.hint?.toLowerCase().includes(q))
    : actions;

  return (
    <Sheet visible={visible} onClose={onClose}>
      <BottomSheetTextInput
        value={query}
        onChangeText={setQuery}
        placeholder="Jump to…"
        placeholderTextColor={theme.textSecondary}
        autoCapitalize="none"
        autoCorrect={false}
        style={[styles.search, { backgroundColor: theme.backgroundElement, color: theme.text }]}
      />
      {filtered.length === 0 ? (
        <EmptyState title="No matches" message={`Nothing matches “${query.trim()}”.`} />
      ) : (
        filtered.map((action) => (
          <Pressable
            key={action.key}
            onPress={go(action.run)}
            style={({ pressed }) => [styles.row, pressed && { backgroundColor: theme.backgroundElement }]}
          >
            <Icon icon={action.icon} size="lg" color={theme.textSecondary} />
            <View style={styles.rowText}>
              <ThemedText numberOfLines={1}>{action.label}</ThemedText>
              {action.hint ? (
                <ThemedText type="small" numberOfLines={1} style={{ color: theme.textSecondary }}>
                  {action.hint}
                </ThemedText>
              ) : null}
            </View>
          </Pressable>
        ))
      )}
    </Sheet>
  );
}

const styles = StyleSheet.create({
  search: {
    borderRadius: Radius.md,
    paddingHorizontal: Spacing.three,
    paddingVertical: Spacing.two,
    fontSize: 16,
    marginBottom: Spacing.one,
  },
  row: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: Spacing.three,
    paddingVertical: Spacing.two,
    paddingHorizontal: Spacing.two,
    borderRadius: Radius.md,
  },
  rowText: { flex: 1, gap: Spacing.half },
});
