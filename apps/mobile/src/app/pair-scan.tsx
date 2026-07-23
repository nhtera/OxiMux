import { CameraView, useCameraPermissions } from 'expo-camera';
import * as Device from 'expo-device';
import { router } from 'expo-router';
import { useCallback, useRef, useState } from 'react';
import { Pressable, StyleSheet, TextInput } from 'react-native';
import { SafeAreaView } from 'react-native-safe-area-context';

import { ThemedText } from '@/components/themed-text';
import { ThemedView } from '@/components/themed-view';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { useClient } from '@/native/client';
import { describeError } from '@/native/errors';

/**
 * The desktop encodes pairing tickets as `oximux://connect?ticket=…`. Anything
 * else is ignored rather than handed to the Rust core, so a stray QR on a poster
 * cannot start a pairing attempt.
 */
const TICKET_PREFIX = 'oximux://connect?ticket=';

export default function PairScanScreen() {
  const theme = useTheme();
  const [permission, requestPermission] = useCameraPermissions();
  const [error, setError] = useState<string>();
  // Simulators have no camera, so scanning can never succeed there. Start such
  // devices in manual entry rather than showing a viewfinder that stays black.
  const [manual, setManual] = useState(!Device.isDevice);
  const [typed, setTyped] = useState('');
  const pair = useClient((s) => s.pair);
  /**
   * The camera fires `onBarcodeScanned` repeatedly while a code is in frame. A
   * ref (not state) latches the first hit, because a state update would not land
   * before the next frame's callback and we would pair several times over.
   */
  const claimed = useRef(false);

  const submit = useCallback(
    async (ticket: string) => {
      const value = ticket.trim();
      if (!value.startsWith(TICKET_PREFIX)) {
        setError('That is not an OxiMux pairing link.');
        return false;
      }
      try {
        await pair(value);
        router.replace('/sessions');
        return true;
      } catch (e) {
        setError(describeError(e));
        return false;
      }
    },
    [pair]
  );

  const onScan = useCallback(
    async ({ data }: { data: string }) => {
      if (claimed.current || !data.startsWith(TICKET_PREFIX)) return;
      claimed.current = true;
      // Let the user retry without leaving the screen if the host refused.
      if (!(await submit(data))) claimed.current = false;
    },
    [submit]
  );

  if (manual) {
    return (
      <ThemedView style={styles.fill}>
        <SafeAreaView style={styles.centered}>
          <ThemedText type="small" style={styles.hint}>
            Paste the pairing link from Settings → Remote on your desktop.
          </ThemedText>
          <TextInput
            value={typed}
            onChangeText={setTyped}
            placeholder="oximux://connect?ticket=…"
            autoCapitalize="none"
            autoCorrect={false}
            multiline
            style={[
              styles.input,
              { borderColor: theme.borderStrong, color: theme.textMuted },
            ]}
          />
          <Pressable onPress={() => submit(typed)} style={styles.button}>
            <ThemedText type="code">Pair</ThemedText>
          </Pressable>
          {Device.isDevice ? (
            <Pressable onPress={() => setManual(false)} style={styles.button}>
              <ThemedText type="small">Scan a QR code instead</ThemedText>
            </Pressable>
          ) : null}
          {error ? (
            <ThemedText type="small" style={styles.hint}>
              {error}
            </ThemedText>
          ) : null}
        </SafeAreaView>
      </ThemedView>
    );
  }

  if (!permission) return <Centered>Checking camera permission…</Centered>;

  if (!permission.granted) {
    return (
      <Centered>
        OxiMux needs the camera to scan the pairing code your desktop shows.
        <Pressable onPress={requestPermission} style={styles.button}>
          <ThemedText type="code">Grant camera access</ThemedText>
        </Pressable>
        <Pressable onPress={() => setManual(true)} style={styles.button}>
          <ThemedText type="small">Paste a link instead</ThemedText>
        </Pressable>
      </Centered>
    );
  }

  return (
    <ThemedView style={styles.fill}>
      <CameraView
        style={styles.fill}
        facing="back"
        barcodeScannerSettings={{ barcodeTypes: ['qr'] }}
        onBarcodeScanned={onScan}
      />
      <SafeAreaView style={styles.overlay}>
        <ThemedText type="small" style={styles.hint}>
          {error ?? 'Point the camera at the pairing code in Settings → Remote.'}
        </ThemedText>
        <Pressable onPress={() => setManual(true)} style={styles.button}>
          <ThemedText type="small">Paste a link instead</ThemedText>
        </Pressable>
      </SafeAreaView>
    </ThemedView>
  );
}

function Centered({ children }: { children: React.ReactNode }) {
  return (
    <ThemedView style={styles.fill}>
      <SafeAreaView style={styles.centered}>
        <ThemedText type="small" style={styles.hint}>
          {children}
        </ThemedText>
      </SafeAreaView>
    </ThemedView>
  );
}

const styles = StyleSheet.create({
  fill: { flex: 1 },
  centered: {
    flex: 1,
    justifyContent: 'center',
    alignItems: 'center',
    gap: Spacing.three,
    paddingHorizontal: Spacing.four,
  },
  overlay: { position: 'absolute', bottom: 0, left: 0, right: 0, padding: Spacing.four },
  hint: { textAlign: 'center' },
  button: { paddingVertical: Spacing.three, paddingHorizontal: Spacing.four },
  input: {
    alignSelf: 'stretch',
    minHeight: 72,
    borderWidth: 1,
    borderRadius: Spacing.two,
    padding: Spacing.three,
  },
});
