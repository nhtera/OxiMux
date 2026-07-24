import Constants from 'expo-constants';
import { router } from 'expo-router';
import { useCallback, useEffect, useState } from 'react';
import { Alert, ScrollView, StyleSheet, View } from 'react-native';
import { SafeAreaView } from 'react-native-safe-area-context';

import { ConnectionBadge } from '@/components/connection-badge';
import { ChatSettings } from '@/components/settings/chat-settings';
import { ThemedText } from '@/components/themed-text';
import { ThemedView } from '@/components/themed-view';
import { Button } from '@/components/ui/button';
import { SegmentedControl } from '@/components/ui/segmented-control';
import { Spacing } from '@/constants/theme';
import { useClient } from '@/native/client';
import { fromBase64 } from '@/native/base64';
import { loadHost, type PairedHost } from '@/native/hosts';
import { useThemePreference, type ThemePreference } from '@/stores/theme-preference';

/**
 * Connection details and the way out of a pairing.
 *
 * `unpair()` has existed on the client store since the shell was built but had no
 * entry point, so a device that reached the session list could never get back to
 * the pair screen — a desktop that changed identity, revoked this device, or
 * simply went away left the app stuck on a dead link with no recovery short of
 * reinstalling. That is the gap this screen closes.
 */
export default function SettingsScreen() {
  const phase = useClient((s) => s.phase);
  const unpair = useClient((s) => s.unpair);
  const [host, setHost] = useState<PairedHost>();

  useEffect(() => {
    void loadHost().then(setHost);
  }, []);

  // Unpairing is destructive enough to confirm — re-pairing needs physical access
  // to the desktop to read a fresh code, so an accidental tap here is not a
  // one-tap recovery.
  const confirmUnpair = useCallback(() => {
    Alert.alert(
      'Forget this desktop?',
      'You will need a new pairing code from the desktop to connect again.',
      [
        { text: 'Cancel', style: 'cancel' },
        {
          text: 'Forget',
          style: 'destructive',
          onPress: () => {
            void (async () => {
              await unpair();
              router.replace('/');
            })();
          },
        },
      ],
    );
  }, [unpair]);

  return (
    <ThemedView style={styles.fill}>
      <SafeAreaView style={styles.fill} edges={['bottom']}>
        <ScrollView contentContainerStyle={styles.body}>
          <View style={styles.section}>
            <ThemedText type="smallBold">Connection</ThemedText>
            <ConnectionBadge />
          </View>

          <View style={styles.section}>
            <ThemedText type="smallBold">Appearance</ThemedText>
            <ThemePicker />
          </View>

          <ChatSettings />

          <View style={styles.section}>
            <ThemedText type="smallBold">Paired desktop</ThemedText>
            {host ? (
              <>
                {/* The endpoint id is the host's public key, so showing it is safe
                    and it is the one value that identifies *which* desktop this is
                    when more than one is in play. Elided in the middle: the ends
                    are what a user compares against the desktop's own display. */}
                <ThemedText type="code" themeColor="textMuted">
                  {shortId(host.endpointId)}
                </ThemedText>
                <ThemedText type="small" themeColor="textMuted">
                  Paired {new Date(host.pairedAt).toLocaleDateString()}
                </ThemedText>
              </>
            ) : (
              <ThemedText type="small" themeColor="textMuted">
                No desktop paired.
              </ThemedText>
            )}
          </View>

          {host ? (
            <Button label="Forget this desktop" variant="danger" onPress={confirmUnpair} />
          ) : (
            <Button
              label="Pair with a desktop"
              variant="primary"
              onPress={() => router.push('/pair-scan')}
            />
          )}

          {phase === 'unreachable' ? (
            <ThemedText type="small" themeColor="textMuted">
              The desktop is not answering. Check that OxiMux is running and that
              Remote access is on in its settings.
            </ThemedText>
          ) : null}

          {Constants.expoConfig?.version ? (
            <ThemedText type="small" themeColor="textMuted" style={styles.version}>
              OxiMux {Constants.expoConfig.version}
            </ThemedText>
          ) : null}
        </ScrollView>
      </SafeAreaView>
    </ThemedView>
  );
}

const THEME_OPTIONS: { value: ThemePreference; label: string }[] = [
  { value: 'system', label: 'System' },
  { value: 'light', label: 'Light' },
  { value: 'dark', label: 'Dark' },
];

/**
 * Three-way theme choice. `System` is listed first because it is the default and
 * the one most people keep.
 */
function ThemePicker() {
  const preference = useThemePreference((s) => s.preference);
  const setPreference = useThemePreference((s) => s.setPreference);

  return (
    <SegmentedControl
      segments={THEME_OPTIONS}
      value={preference}
      onChange={setPreference}
      accessibilityLabel="Theme"
    />
  );
}

/**
 * The endpoint id as the desktop prints it: first four and last four bytes in
 * hex, middle elided.
 *
 * The bytes are stored base64 here but the desktop's Remote pane renders hex, and
 * the whole point of showing this is that a user can check the two match. Slicing
 * the base64 string would have produced something that looks like an id and never
 * agrees with the desktop, so it decodes first.
 */
function shortId(endpointId: string): string {
  const bytes = fromBase64(endpointId);
  const hex = (slice: Uint8Array) =>
    Array.from(slice, (b) => b.toString(16).padStart(2, '0')).join('');
  return `${hex(bytes.slice(0, 4))}…${hex(bytes.slice(-4))}`;
}

const styles = StyleSheet.create({
  fill: { flex: 1 },
  body: { padding: Spacing.four, gap: Spacing.five },
  section: { gap: Spacing.two },
  version: { textAlign: 'center' },
});
