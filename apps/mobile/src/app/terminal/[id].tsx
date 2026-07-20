import { Stack, useLocalSearchParams } from 'expo-router';
import { ActivityIndicator, KeyboardAvoidingView, Platform, Pressable, StyleSheet, View } from 'react-native';
import { SafeAreaView, useSafeAreaInsets } from 'react-native-safe-area-context';

import { TerminalView } from '@/components/terminal/terminal-view';
import { ThemedText } from '@/components/themed-text';
import { ThemedView } from '@/components/themed-view';
import { Spacing } from '@/constants/theme';
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
      <KeyboardAvoidingView
        style={styles.fill}
        behavior={Platform.OS === 'ios' ? 'padding' : undefined}
        keyboardVerticalOffset={Platform.OS === 'ios' ? keyboardOffset : 0}
      >
        <SafeAreaView style={styles.fill} edges={['bottom']}>
          {error ? (
            <Pressable onPress={dismissError} style={styles.error}>
              <ThemedText type="small" style={styles.errorText}>
                {error} — tap to dismiss
              </ThemedText>
            </Pressable>
          ) : null}

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
  error: { paddingHorizontal: Spacing.three, paddingVertical: Spacing.two },
  errorText: { color: '#F85149' },
});
