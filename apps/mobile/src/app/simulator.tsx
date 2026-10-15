import { router } from 'expo-router';
import { Smartphone } from 'lucide-react-native';
import type { SimViewable } from 'oximux-core';
import { FlatList, Pressable, RefreshControl, StyleSheet, View } from 'react-native';
import { SafeAreaView } from 'react-native-safe-area-context';

import { ConnectionBanner } from '@/components/connection-banner';
import { ThemedText } from '@/components/themed-text';
import { ThemedView } from '@/components/themed-view';
import { Badge } from '@/components/ui/badge';
import { EmptyState } from '@/components/ui/empty-state';
import { ErrorBanner } from '@/components/ui/error-banner';
import { ListDivider } from '@/components/ui/list-row';
import { SkeletonList } from '@/components/ui/skeleton';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { tick } from '@/native/haptics';
import { useSimViewables } from '@/native/simulator';

/**
 * The devices on the desktop this phone may watch (never drive).
 *
 * Viewing is off on the desktop until its user turns it on, and that is
 * explained here rather than shown as an empty list: "not shared" and "none
 * attached" would look the same otherwise.
 */
export default function DevicesScreen() {
  const { devices, refused, loading, error, reload } = useSimViewables();
  const theme = useTheme();

  return (
    <ThemedView style={styles.fill}>
      <SafeAreaView style={styles.fill} edges={['bottom']}>
        <ConnectionBanner />
        {error ? (
          <View style={styles.notice}>
            <ErrorBanner message={error} onDismiss={reload} />
            <ThemedText type="small" style={{ color: theme.textSecondary }}>
              {error.includes('not authorized') || error.includes('Unauthorized')
                ? 'This phone is limited to one agent session, so devices are not shared with it. Change its access on the desktop, in Settings → Remote.'
                : 'Tap to try again.'}
            </ThemedText>
          </View>
        ) : null}
        {loading ? (
          <SkeletonList />
        ) : (
          <FlatList
            data={devices}
            keyExtractor={(d) => d.device}
            renderItem={({ item }) => <DeviceRow device={item} />}
            ItemSeparatorComponent={ListDivider}
            contentContainerStyle={styles.list}
            refreshControl={<RefreshControl refreshing={false} onRefresh={reload} />}
            ListEmptyComponent={
              error ? null : (
                <EmptyState
                  icon={Smartphone}
                  title={refused ? 'Viewing is off on the desktop' : 'No devices attached on the desktop.'}
                  message={refused}
                />
              )
            }
          />
        )}
      </SafeAreaView>
    </ThemedView>
  );
}

function classLabel(device: SimViewable): string {
  switch (device.class) {
    case 'ios-simulator':
      return 'iOS Simulator';
    case 'android-emulator':
      return 'Android emulator';
    case 'iphone':
      return 'iPhone';
    default:
      return 'Android phone';
  }
}

function DeviceRow({ device }: { device: SimViewable }) {
  const theme = useTheme();
  return (
    <Pressable
      disabled={!device.streaming}
      onPress={() => {
        tick();
        router.push({ pathname: '/simulator/[device]', params: { device: device.device, name: device.name } });
      }}
      style={({ pressed }) => [styles.row, pressed && { backgroundColor: theme.surface2 }, !device.streaming && styles.dim]}
    >
      <View style={styles.rowText}>
        <ThemedText type="smallBold">{device.name}</ThemedText>
        <ThemedText type="small" style={{ color: theme.textSecondary }}>
          {device.worktree} · {classLabel(device)}
        </ThemedText>
      </View>
      <Badge label={device.streaming ? 'Streaming' : 'Not streaming'} tone={device.streaming ? 'success' : 'neutral'} />
    </Pressable>
  );
}

const styles = StyleSheet.create({
  fill: { flex: 1 },
  list: { flexGrow: 1, paddingVertical: Spacing.two },
  row: { flexDirection: 'row', alignItems: 'center', gap: Spacing.two, paddingVertical: Spacing.three, paddingHorizontal: Spacing.four },
  rowText: { flex: 1, gap: Spacing.one },
  dim: { opacity: 0.55 },
  notice: { paddingHorizontal: Spacing.three, paddingVertical: Spacing.two, gap: Spacing.one },
});
