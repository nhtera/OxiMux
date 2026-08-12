import { Stack, useLocalSearchParams } from 'expo-router';
import { Smartphone, Monitor } from 'lucide-react-native';
import { useCallback, useRef, useState } from 'react';
import { ActivityIndicator, Pressable, StyleSheet, View } from 'react-native';
import { KeyboardAvoidingView } from 'react-native-keyboard-controller';
import { SafeAreaView, useSafeAreaInsets } from 'react-native-safe-area-context';

import { ConnectionBanner } from '@/components/connection-banner';
import { KeyBar } from '@/components/terminal/key-bar';
import type { FitMode } from '@/components/terminal/terminal-page';
import { TerminalView, type Geometry, type TerminalHandle } from '@/components/terminal/terminal-view';
import { ThemedText } from '@/components/themed-text';
import { ThemedView } from '@/components/themed-view';
import { Icon } from '@/components/ui/icon';
import { ErrorBanner } from '@/components/ui/error-banner';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { tick } from '@/native/haptics';
import { useTerminal } from '@/native/terminal';

/**
 * One attached terminal.
 *
 * Everything on screen is drawn by a real emulator from the host's raw bytes —
 * there is no second model of the screen on this side to drift out of sync with
 * what the user sees.
 */
export default function TerminalScreen() {
  const { id } = useLocalSearchParams<{ id: string }>();
  const ptyId = String(id);
  const { subscribe, error, loading, send, resize, dismissError } = useTerminal(ptyId);
  const insets = useSafeAreaInsets();
  const term = useRef<TerminalHandle>(null);
  const [mode, setMode] = useState<FitMode>('mirror');
  const [geometry, setGeometry] = useState<Geometry>();
  const [ctrlArmed, setCtrlArmed] = useState(false);
  // Same reasoning as the chat screen: the stack header sits above this view, so
  // the keyboard has to clear it or it pushes the grid behind the header.
  const keyboardOffset = insets.top + 44;

  // A key-bar tap lands outside the WebView, which blurs xterm's textarea and
  // drops the soft keyboard. Re-focusing after every send keeps typing where the
  // user left it.
  const key = useCallback(
    (sequence: string) => {
      send(sequence);
      term.current?.focus();
    },
    [send]
  );

  // Every refit reports geometry, and most reports say the same thing. Setting
  // state regardless re-renders the screen — and therefore the WebView — for no
  // visible change, which is exactly the churn that used to reload the page.
  const reportGeometry = useCallback((next: Geometry) => {
    setGeometry((prev) =>
      prev &&
      prev.cols === next.cols &&
      prev.rows === next.rows &&
      prev.fontSize === next.fontSize &&
      prev.overflow === next.overflow
        ? prev
        : next
    );
  }, []);

  const toggleCtrl = useCallback(() => {
    setCtrlArmed((on) => {
      term.current?.setCtrl(!on);
      return !on;
    });
  }, []);

  return (
    <ThemedView style={styles.fill}>
      <Stack.Screen
        options={{
          title: 'Terminal',
          headerRight: () => (
            <FitToggle mode={mode} onChange={setMode} />
          ),
        }}
      />
      <KeyboardAvoidingView style={styles.fill} behavior="padding" keyboardVerticalOffset={keyboardOffset}>
        <SafeAreaView style={styles.fill} edges={['bottom']}>
          <ConnectionBanner />
          {error ? <ErrorBanner message={error} onDismiss={dismissError} /> : null}

          {loading ? (
            <View style={styles.centre}>
              <ActivityIndicator />
            </View>
          ) : (
            <>
              <TerminalView
                ref={term}
                subscribe={subscribe}
                onInput={send}
                onResize={resize}
                onGeometry={reportGeometry}
                onCtrlChange={setCtrlArmed}
                mode={mode}
              />
              <GeometryHint geometry={geometry} mode={mode} />
              <KeyBar
                onKey={key}
                onCtrlToggle={toggleCtrl}
                onZoom={(delta) => term.current?.zoom(delta)}
                onScroll={(lines) => term.current?.scroll(lines)}
                ctrlArmed={ctrlArmed}
              />
            </>
          )}
        </SafeAreaView>
      </KeyboardAvoidingView>
    </ThemedView>
  );
}

/**
 * Mirror vs reflow.
 *
 * Worth a visible control rather than a settings toggle, because the two modes
 * differ in whether watching from the phone changes what the desktop shows: the
 * relay runs each PTY at the smallest size any attachment asks for, so reflow
 * narrows the desktop's terminal to phone width for as long as the phone is
 * attached. Mirror is the default for exactly that reason.
 */
function FitToggle({ mode, onChange }: { mode: FitMode; onChange: (mode: FitMode) => void }) {
  const theme = useTheme();
  const mirroring = mode === 'mirror';
  return (
    <Pressable
      accessibilityRole="button"
      accessibilityLabel={mirroring ? 'Mirroring the desktop size' : 'Fitted to this phone'}
      accessibilityState={{ selected: !mirroring }}
      hitSlop={8}
      onPress={() => {
        tick();
        onChange(mirroring ? 'reflow' : 'mirror');
      }}
      style={({ pressed }) => [styles.toggle, pressed && styles.pressed]}
    >
      <Icon icon={mirroring ? Monitor : Smartphone} size="md" color={theme.text} />
      <ThemedText type="small" style={{ color: theme.textSecondary }}>
        {mirroring ? 'Desktop' : 'Phone'}
      </ThemedText>
    </Pressable>
  );
}

/**
 * The grid's size and, in mirror mode, whether it is wider than the screen.
 *
 * Columns off the right edge are otherwise invisible — the page pans, but
 * nothing says there is anything to pan to.
 */
function GeometryHint({ geometry, mode }: { geometry?: Geometry; mode: FitMode }) {
  const theme = useTheme();
  if (!geometry) return null;
  const clipped = mode === 'mirror' && geometry.overflow;
  return (
    <View style={[styles.hint, { borderTopColor: theme.border }]}>
      <ThemedText type="small" style={{ color: theme.textSecondary }}>
        {geometry.cols}×{geometry.rows}
        {clipped ? ' · swipe for the rest' : null}
      </ThemedText>
    </View>
  );
}

const styles = StyleSheet.create({
  fill: { flex: 1 },
  centre: { flex: 1, alignItems: 'center', justifyContent: 'center' },
  toggle: { flexDirection: 'row', alignItems: 'center', gap: Spacing.one, paddingHorizontal: Spacing.one },
  pressed: { opacity: 0.6 },
  hint: {
    borderTopWidth: StyleSheet.hairlineWidth,
    paddingHorizontal: Spacing.three,
    paddingVertical: Spacing.one,
  },
});
