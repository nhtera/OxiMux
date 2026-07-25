import { CameraView, useCameraPermissions } from 'expo-camera';
import * as Device from 'expo-device';
import { router } from 'expo-router';
import { useCallback, useRef, useState } from 'react';
import { StyleSheet, TextInput, View } from 'react-native';
import { SafeAreaView } from 'react-native-safe-area-context';

import { ThemedText } from '@/components/themed-text';
import { ThemedView } from '@/components/themed-view';
import { Button } from '@/components/ui/button';
import { ErrorBanner } from '@/components/ui/error-banner';
import { Radius, Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { useClient } from '@/native/client';
import { describeError } from '@/native/errors';

/**
 * The desktop encodes pairing tickets as `oximux://connect?ticket=…`. Anything
 * else is ignored rather than handed to the Rust core, so a stray QR on a poster
 * cannot start a pairing attempt.
 */
const TICKET_PREFIX = 'oximux://connect?ticket=';

/** Side of the viewfinder cutout. Large enough that a desktop screen's code
 * fills it from a comfortable arm's length. */
const FRAME = 240;

export default function PairScanScreen() {
  const [permission, requestPermission] = useCameraPermissions();
  const [error, setError] = useState<string>();
  // Simulators have no camera, so scanning can never succeed there. Such devices
  // start in manual entry rather than showing a viewfinder that stays black —
  // which is why the paste form, not the camera, is what a simulator ever shows.
  const [manual, setManual] = useState(!Device.isDevice);
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
      setError(undefined);
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
    ({ data }: { data: string }) => {
      if (claimed.current || !data.startsWith(TICKET_PREFIX)) return;
      claimed.current = true;
      // Let the user retry without leaving the screen if the host refused.
      void submit(data).then((ok) => {
        if (!ok) claimed.current = false;
      });
    },
    [submit]
  );

  if (manual) {
    return (
      <ManualEntry
        error={error}
        onDismissError={() => setError(undefined)}
        onSubmit={submit}
        onScanInstead={Device.isDevice ? () => setManual(false) : undefined}
      />
    );
  }

  if (!permission) {
    return <Prompt message="Checking camera permission…" />;
  }

  if (!permission.granted) {
    return (
      <Prompt message="OxiMux needs the camera to scan the pairing code your desktop shows.">
        <Button label="Grant camera access" variant="primary" onPress={requestPermission} />
        <Button label="Paste a link instead" variant="ghost" onPress={() => setManual(true)} />
      </Prompt>
    );
  }

  return (
    <ThemedView style={styles.fill}>
      <CameraView
        style={StyleSheet.absoluteFill}
        facing="back"
        barcodeScannerSettings={{ barcodeTypes: ['qr'] }}
        onBarcodeScanned={onScan}
      />
      {/* A dimmed surround with a clear square cut out of it, built from plain
          views rather than a mask: it tells the user where to aim without
          obscuring the part of the frame the scanner is reading. */}
      <SafeAreaView style={styles.fill}>
        <View style={styles.scrim} />
        <View style={styles.frameRow}>
          <View style={styles.scrim} />
          <View style={styles.frame} />
          <View style={styles.scrim} />
        </View>
        <View style={[styles.scrim, styles.foot]}>
          {error ? <ErrorBanner message={error} onDismiss={() => setError(undefined)} /> : null}
          <ThemedText type="small" style={styles.hint}>
            Point the camera at the code in Settings → Remote on your desktop.
          </ThemedText>
          <Button label="Paste a link instead" variant="ghost" onPress={() => setManual(true)} />
        </View>
      </SafeAreaView>
    </ThemedView>
  );
}

/** Pasting the link, for a device with no camera or no permission for it. */
function ManualEntry({
  error,
  onDismissError,
  onSubmit,
  onScanInstead,
}: {
  error?: string;
  onDismissError: () => void;
  onSubmit: (ticket: string) => Promise<boolean>;
  onScanInstead?: () => void;
}) {
  const theme = useTheme();
  const [typed, setTyped] = useState('');

  return (
    <ThemedView style={styles.fill}>
      <SafeAreaView style={styles.centered}>
        <ThemedText type="small" style={styles.hint}>
          {onScanInstead
            ? 'Paste the pairing link from Settings → Remote on your desktop.'
            : 'This device has no camera, so paste the pairing link from Settings → Remote on your desktop instead.'}
        </ThemedText>
        <TextInput
          value={typed}
          onChangeText={setTyped}
          placeholder="oximux://connect?ticket=…"
          placeholderTextColor={theme.textMuted}
          autoCapitalize="none"
          autoCorrect={false}
          multiline
          // The typed link is content, not a placeholder: it was muted before, so
          // a pasted ticket read as greyed-out and unusable.
          style={[styles.input, { borderColor: theme.borderStrong, color: theme.text }]}
        />
        {error ? <ErrorBanner message={error} onDismiss={onDismissError} /> : null}
        <Button
          label="Pair"
          variant="primary"
          disabled={typed.trim().length === 0}
          onPress={() => void onSubmit(typed)}
          style={styles.stretch}
        />
        {onScanInstead ? (
          <Button label="Scan a QR code instead" variant="ghost" onPress={onScanInstead} />
        ) : null}
      </SafeAreaView>
    </ThemedView>
  );
}

/**
 * A centred message with optional actions beneath it.
 *
 * The actions are siblings of the text, not children of it: they used to be
 * passed *into* a `<ThemedText>`, which nests pressables inside a `<Text>` — a
 * combination React Native lays out oddly on iOS and whose touches do not
 * reliably land on Android.
 */
function Prompt({ message, children }: { message: string; children?: React.ReactNode }) {
  return (
    <ThemedView style={styles.fill}>
      <SafeAreaView style={styles.centered}>
        <ThemedText type="small" style={styles.hint}>
          {message}
        </ThemedText>
        {children}
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
  stretch: { alignSelf: 'stretch' },
  hint: { textAlign: 'center' },
  scrim: { flex: 1, backgroundColor: 'rgba(0, 0, 0, 0.55)' },
  frameRow: { flexDirection: 'row', height: FRAME },
  frame: {
    width: FRAME,
    borderWidth: 2,
    borderColor: 'rgba(255, 255, 255, 0.9)',
    borderRadius: Radius.lg,
  },
  foot: {
    justifyContent: 'flex-start',
    alignItems: 'center',
    gap: Spacing.three,
    padding: Spacing.four,
  },
  input: {
    alignSelf: 'stretch',
    minHeight: 72,
    borderWidth: 1,
    borderRadius: Radius.md,
    padding: Spacing.three,
  },
});
