import { CameraView, useCameraPermissions } from 'expo-camera';
import * as Device from 'expo-device';
import { router } from 'expo-router';
import { useCallback, useEffect, useRef, useState } from 'react';
import { ActivityIndicator, StyleSheet, TextInput, View } from 'react-native';
import { SafeAreaView } from 'react-native-safe-area-context';

import { ThemedText } from '@/components/themed-text';
import { ThemedView } from '@/components/themed-view';
import { Button } from '@/components/ui/button';
import { ErrorBanner } from '@/components/ui/error-banner';
import { Radius, Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { useClient, type PairStep } from '@/native/client';
import { describePairingError } from '@/native/errors';

/**
 * The desktop encodes pairing tickets as `oximux://connect?ticket=…`. Anything
 * else is ignored rather than handed to the Rust core, so a stray QR on a poster
 * cannot start a pairing attempt.
 */
const TICKET_PREFIX = 'oximux://connect?ticket=';

/** Side of the viewfinder cutout. Large enough that a desktop screen's code
 * fills it from a comfortable arm's length. */
const FRAME = 240;

/**
 * How long to wait on the host before calling the attempt stuck.
 *
 * Generous, because a first dial across networks legitimately takes seconds —
 * this is the boundary between "slow" and "never", not a latency budget. Without
 * it the screen has no failure state at all: the promise simply never settles and
 * the scanner stays latched, which is the hang this screen was reported for.
 */
const PAIR_TIMEOUT_MS = 30_000;

/** When a wait stops looking like progress and starts looking broken. */
const SLOW_AFTER_MS = 3_000;

/** What the screen is doing, as far as the person holding the phone is concerned. */
type Progress =
  | { kind: 'idle' }
  | { kind: 'working'; step: PairStep }
  | { kind: 'failed'; message: string };

/**
 * Reject if `work` has not settled within `ms`.
 *
 * This bounds the *screen*, not the dial: the Rust client keeps going and may
 * still connect afterwards. That result is deliberately dropped (see the
 * generation guard in `submit`) — a success surfacing after the user has been
 * told it failed is worse than a wasted dial, and the next attempt supersedes
 * the client anyway.
 */
function withTimeout<T>(work: Promise<T>, ms: number): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('timed out')), ms);
    work.then(resolve, reject).finally(() => clearTimeout(timer));
  });
}

export default function PairScanScreen() {
  const [permission, requestPermission] = useCameraPermissions();
  const [progress, setProgress] = useState<Progress>({ kind: 'idle' });
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
  /**
   * Which attempt is current. A timed-out or cancelled attempt keeps running in
   * the Rust core, so every callback and result checks this before touching
   * state — otherwise an abandoned dial could navigate away from, or overwrite,
   * the attempt the user is actually watching.
   */
  const attempt = useRef(0);

  const submit = useCallback(
    async (ticket: string) => {
      const value = ticket.trim();
      if (!value.startsWith(TICKET_PREFIX)) {
        setProgress({ kind: 'failed', message: "That isn't an OxiMux pairing link." });
        return false;
      }
      const generation = ++attempt.current;
      const current = () => attempt.current === generation;
      setProgress({ kind: 'working', step: 'connecting' });
      try {
        await withTimeout(
          pair(value, (step) => {
            if (current()) setProgress({ kind: 'working', step });
          }),
          PAIR_TIMEOUT_MS
        );
        if (!current()) return false;
        router.replace('/sessions');
        return true;
      } catch (e) {
        if (!current()) return false;
        setProgress({ kind: 'failed', message: describePairingError(e) });
        return false;
      }
    },
    [pair]
  );

  /** Abandon the in-flight attempt and return to the viewfinder. */
  const cancel = useCallback(() => {
    attempt.current += 1;
    claimed.current = false;
    setProgress({ kind: 'idle' });
  }, []);

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

  // The camera is irrelevant once a code has been read — it is still telling the
  // user to aim at something they have already captured. Replacing it with the
  // attempt's own state is the fix for "it looks stuck": before this, a scan
  // changed nothing on screen at all.
  if (progress.kind === 'working') {
    // Keyed by step so moving to `resuming` remounts and restarts its own
    // slow-wait timer, rather than inheriting one already most of the way down.
    return <PairingProgress key={progress.step} step={progress.step} onCancel={cancel} />;
  }
  if (progress.kind === 'failed' && !manual) {
    return (
      <PairingFailed
        message={progress.message}
        onRetry={cancel}
        onManual={() => {
          claimed.current = false;
          setManual(true);
        }}
      />
    );
  }

  if (manual) {
    return (
      <ManualEntry
        error={progress.kind === 'failed' ? progress.message : undefined}
        onDismissError={() => setProgress({ kind: 'idle' })}
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
          <ThemedText type="small" style={styles.hint}>
            Point the camera at the code in Settings → Remote on your desktop.
          </ThemedText>
          <Button label="Paste a link instead" variant="ghost" onPress={() => setManual(true)} />
        </View>
      </SafeAreaView>
    </ThemedView>
  );
}

/**
 * What the phone is doing between the scan and the result.
 *
 * The whole screen, not an overlay on the viewfinder: after a successful read
 * the camera has nothing left to contribute, and leaving it behind a spinner
 * keeps asking the user to aim at a code that has already been captured.
 */
function PairingProgress({ step, onCancel }: { step: PairStep; onCancel: () => void }) {
  const theme = useTheme();
  const [slow, setSlow] = useState(false);

  // A wait you were told to expect reads as progress; the same wait unannounced
  // reads as a hang. Held back a few seconds so a fast pairing never flashes it.
  //
  // No reset on `step` here — the caller keys this component by step, so a new
  // step remounts and the timer starts over on its own. Resetting in the effect
  // body instead would be a cascading render for the same result.
  useEffect(() => {
    const timer = setTimeout(() => setSlow(true), SLOW_AFTER_MS);
    return () => clearTimeout(timer);
  }, []);

  return (
    <ThemedView style={styles.fill}>
      <SafeAreaView style={styles.centered}>
        <ActivityIndicator size="large" color={theme.accent} />
        <ThemedText type="smallBold" style={styles.hint}>
          {step === 'resuming'
            ? 'Already paired with this Mac — reconnecting instead…'
            : 'Connecting to your Mac…'}
        </ThemedText>
        {slow ? (
          <ThemedText type="small" themeColor="textMuted" style={styles.hint}>
            Reaching a Mac on another network goes through a relay, which can take a few seconds.
          </ThemedText>
        ) : null}
        <Button label="Cancel" variant="ghost" onPress={onCancel} />
      </SafeAreaView>
    </ThemedView>
  );
}

/**
 * A pairing that did not work, and the two things worth trying next.
 *
 * Its own screen rather than the banner this used to be: that banner sat in the
 * dimmed foot of a live viewfinder, which is where a message goes to be missed.
 */
function PairingFailed({
  message,
  onRetry,
  onManual,
}: {
  message: string;
  onRetry: () => void;
  onManual: () => void;
}) {
  return (
    <ThemedView style={styles.fill}>
      <SafeAreaView style={styles.centered}>
        <ThemedText type="smallBold" style={styles.hint}>
          Couldn&apos;t pair
        </ThemedText>
        <ThemedText type="small" themeColor="textMuted" style={styles.hint}>
          {message}
        </ThemedText>
        <Button label="Try again" variant="primary" onPress={onRetry} style={styles.stretch} />
        <Button label="Paste a link instead" variant="ghost" onPress={onManual} />
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
