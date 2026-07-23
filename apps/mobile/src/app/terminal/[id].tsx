import { Stack, useLocalSearchParams } from 'expo-router';
import { ActivityIndicator, StyleSheet, View } from 'react-native';
import { KeyboardAvoidingView } from 'react-native-keyboard-controller';
import { SafeAreaView, useSafeAreaInsets } from 'react-native-safe-area-context';

import { ConnectionBanner } from '@/components/connection-banner';
import { TerminalView } from '@/components/terminal/terminal-view';
import { ThemedView } from '@/components/themed-view';
import { ErrorBanner } from '@/components/ui/error-banner';
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
  const { pending, error, loading, send, resize, dismissError } = useTerminal(ptyId);
  const insets = useSafeAreaInsets();
  // Same reasoning as the chat screen: the stack header sits above this view, so
  // the keyboard has to clear it or it pushes the grid behind the header.
  const keyboardOffset = insets.top + 44;

  return (
    <ThemedView style={styles.fill}>
      <Stack.Screen options={{ title: 'Terminal' }} />
      <KeyboardAvoidingView style={styles.fill} behavior="padding" keyboardVerticalOffset={keyboardOffset}>
        <SafeAreaView style={styles.fill} edges={['bottom']}>
          <ConnectionBanner />
          {error ? <ErrorBanner message={error} onDismiss={dismissError} /> : null}

          {loading ? (
            <View style={styles.centre}>
              <ActivityIndicator />
            </View>
          ) : (
            <TerminalView frames={pending} onInput={send} onResize={resize} />
          )}
        </SafeAreaView>
      </KeyboardAvoidingView>
    </ThemedView>
  );
}

const styles = StyleSheet.create({
  fill: { flex: 1 },
  centre: { flex: 1, alignItems: 'center', justifyContent: 'center' },
});
