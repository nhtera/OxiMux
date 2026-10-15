import { Stack, router, useFocusEffect, useLocalSearchParams } from 'expo-router';
import { useCallback, useEffect, useState } from 'react';
import { ActivityIndicator, AppState, StyleSheet, View } from 'react-native';
import { SafeAreaView } from 'react-native-safe-area-context';

import { SimView } from '@/components/simulator/sim-view';
import { ThemedText } from '@/components/themed-text';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Spacing } from '@/constants/theme';
import { useSimView, type ViewPhase } from '@/native/simulator';

/**
 * One device on the desktop, live and full screen. Watching only.
 *
 * The view runs only while this screen is focused and the app is in front:
 * leaving it, locking the phone or switching apps unsubscribes at once (the
 * desktop's "Viewed from" indicator goes with it), and coming back
 * subscribes again. Nothing is recorded; frames are drawn and dropped.
 */
export default function DeviceViewScreen() {
  const { device, name } = useLocalSearchParams<{ device: string; name?: string }>();
  const [focused, setFocused] = useState(false);
  const [foreground, setForeground] = useState(AppState.currentState === 'active');
  const [shown, setShown] = useState(false);
  const [unsupported, setUnsupported] = useState(false);

  useFocusEffect(
    useCallback(() => {
      setFocused(true);
      return () => setFocused(false);
    }, [])
  );
  useEffect(() => {
    const sub = AppState.addEventListener('change', (state) => setForeground(state === 'active'));
    return () => sub.remove();
  }, []);

  const active = focused && foreground && !unsupported;
  const { phase, relayed, subscribe, retry, restart } = useSimView(String(device), active);
  // The page has drawn, and the view is live: the picture is real.
  const pictured = shown && phase.kind === 'live';

  return (
    <View style={styles.fill}>
      <Stack.Screen options={{ title: name ? String(name) : 'Device' }} />
      <SimView
        subscribe={subscribe}
        onFirstFrame={() => setShown(true)}
        onUnsupported={() => setUnsupported(true)}
        onStuck={restart}
      />
      <SafeAreaView style={styles.overlay} edges={['bottom']} pointerEvents="box-none">
        {/* Always laid out, so the bar stays at the bottom; dimmed over a
            picture that is no longer live, so the notice reads on any frame. */}
        <View style={[styles.center, (!pictured || unsupported) && styles.dim]} pointerEvents="box-none">
          {!pictured || unsupported ? <Notice phase={phase} unsupported={unsupported} onRetry={retry} /> : null}
        </View>
        <View style={styles.bar}>
          {relayed ? <Badge label="Relayed" tone="warning" /> : <View />}
          <Button label="Stop" variant="secondary" size="compact" onPress={() => router.back()} />
        </View>
      </SafeAreaView>
    </View>
  );
}

function Notice({ phase, unsupported, onRetry }: { phase: ViewPhase; unsupported: boolean; onRetry: () => void }) {
  if (unsupported) {
    return <Message text="This phone's web view cannot decode the device's video." />;
  }
  switch (phase.kind) {
    case 'starting':
    case 'live':
      return <ActivityIndicator color="#fff" />;
    case 'paused':
      return null;
    case 'waiting':
      return (
        <>
          <ActivityIndicator color="#fff" />
          <Message text={phase.message} />
        </>
      );
    case 'denied':
    case 'refused':
    case 'error':
      return (
        <>
          <Message text={phase.message} />
          <Button label="Try again" variant="secondary" size="compact" onPress={onRetry} />
        </>
      );
  }
}

function Message({ text }: { text: string }) {
  return (
    <ThemedText type="small" style={styles.message}>
      {text}
    </ThemedText>
  );
}

const styles = StyleSheet.create({
  fill: { flex: 1, backgroundColor: '#000' },
  overlay: { ...StyleSheet.absoluteFill, justifyContent: 'space-between' },
  center: { flex: 1, alignItems: 'center', justifyContent: 'center', gap: Spacing.two, paddingHorizontal: Spacing.four },
  dim: { backgroundColor: 'rgba(0,0,0,0.6)' },
  message: { color: '#fff', textAlign: 'center' },
  bar: {
    flexDirection: 'row',
    alignItems: 'center',
    justifyContent: 'space-between',
    paddingHorizontal: Spacing.three,
    paddingVertical: Spacing.two,
  },
});
