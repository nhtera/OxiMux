import { Stack, router, useLocalSearchParams } from 'expo-router';
import { ForgeItemKind, type ForgeItem } from 'oximux-core';
import { useState } from 'react';
import {
  ActivityIndicator,
  FlatList,
  Pressable,
  RefreshControl,
  StyleSheet,
  View,
} from 'react-native';
import { SafeAreaView } from 'react-native-safe-area-context';

import { CheckList } from '@/components/forge/check-list';
import { ForgeItemRow } from '@/components/forge/item-row';
import { ThemedText } from '@/components/themed-text';
import { ThemedView } from '@/components/themed-view';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { attachPromptText } from '@/native/forge';
import { useForge } from '@/native/use-forge';

/**
 * Issues, pull requests and CI checks for the repository a session lives in.
 *
 * Everything here is read-only. Creating or merging a pull request is
 * deliberately absent: a merge is effectively irreversible, and that is a
 * decision worth making at a desk rather than on a lock screen.
 *
 * No credential lives on this device — the desktop runs the forge CLI already
 * signed in there, so this screen has no sign-in state to manage or lose.
 */
export default function ForgeScreen() {
  const { id } = useLocalSearchParams<{ id: string }>();
  const sessionId = String(id);
  const [kind, setKind] = useState<ForgeItemKind>(ForgeItemKind.Pull);
  const { items, checks, loading, error, refresh, detail } = useForge(sessionId, kind);

  /**
   * Attach an item's context to the composer and go back to the chat.
   *
   * The body is fetched first because the listing omits it, and an attachment
   * carrying only a title and link is much less useful to the agent than one
   * carrying the description. It is passed as ordinary prompt text through the
   * existing send path — nothing about attaching needs its own RPC.
   */
  const attach = async (item: ForgeItem) => {
    const body = await detail(item.number);
    router.navigate({
      pathname: '/session/[id]',
      params: { id: sessionId, draft: attachPromptText(item, body) },
    });
  };

  return (
    <ThemedView style={styles.fill}>
      <Stack.Screen options={{ title: 'Pull requests & issues' }} />
      <SafeAreaView style={styles.fill} edges={['bottom']}>
        <KindToggle kind={kind} onChange={setKind} />

        {error ? (
          <ThemedText type="small" style={styles.error} numberOfLines={3}>
            {error}
          </ThemedText>
        ) : null}

        {loading && items.length === 0 ? (
          <View style={styles.centre}>
            <ActivityIndicator />
          </View>
        ) : (
          <FlatList
            data={items}
            keyExtractor={(item) => String(item.number)}
            ListHeaderComponent={<CheckList checks={checks} />}
            renderItem={({ item }) => (
              <ForgeItemRow
                item={item}
                onPress={() => attach(item)}
                onAttach={() => attach(item)}
              />
            )}
            refreshControl={<RefreshControl refreshing={loading} onRefresh={refresh} />}
            ListEmptyComponent={
              <View style={styles.centre}>
                <ThemedText type="small" style={styles.empty}>
                  {/* Deliberately vague about the cause. The desktop cannot tell
                      "no open items" from "no gh installed", "gh signed out" or
                      "not a GitHub/GitLab repo" — so naming one would be a
                      guess presented as fact. */}
                  Nothing to show. This repository may not be hosted on GitHub or
                  GitLab, or its CLI may not be set up on the desktop.
                </ThemedText>
              </View>
            }
          />
        )}
      </SafeAreaView>
    </ThemedView>
  );
}

function KindToggle({
  kind,
  onChange,
}: {
  kind: ForgeItemKind;
  onChange: (k: ForgeItemKind) => void;
}) {
  const theme = useTheme();
  return (
    <View style={styles.toggle}>
      {(
        [
          [ForgeItemKind.Pull, 'Pull requests'],
          [ForgeItemKind.Issue, 'Issues'],
        ] as const
      ).map(([value, label]) => (
        <Pressable
          key={label}
          onPress={() => onChange(value)}
          style={[
            styles.tab,
            kind === value && { backgroundColor: theme.backgroundSelected },
          ]}
        >
          <ThemedText type="code">{label}</ThemedText>
        </Pressable>
      ))}
    </View>
  );
}

const styles = StyleSheet.create({
  fill: { flex: 1 },
  centre: { flex: 1, alignItems: 'center', justifyContent: 'center', padding: Spacing.four },
  toggle: { flexDirection: 'row', gap: Spacing.two, padding: Spacing.three },
  tab: {
    paddingVertical: Spacing.two,
    paddingHorizontal: Spacing.three,
    borderRadius: Spacing.two,
  },
  empty: { opacity: 0.7, textAlign: 'center' },
  error: { color: '#F85149', paddingHorizontal: Spacing.three, paddingVertical: Spacing.two },
});
