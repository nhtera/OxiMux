import { Link, Stack, useLocalSearchParams } from 'expo-router';
import { ActivityIndicator, KeyboardAvoidingView, Platform, Pressable, StyleSheet, View } from 'react-native';
import { SafeAreaView, useSafeAreaInsets } from 'react-native-safe-area-context';

import { Composer } from '@/components/chat/composer';
import { StatusStrip } from '@/components/chat/status-strip';
import { Transcript } from '@/components/chat/transcript';
import { ThemedText } from '@/components/themed-text';
import { ThemedView } from '@/components/themed-view';
import { Spacing } from '@/constants/theme';
import { useSession } from '@/native/session';

/**
 * The core screen: one agent session, live. The transcript arrives already
 * folded from the Rust core, so this composes panels rather than reducing
 * events.
 */
export default function SessionScreen() {
  const { id } = useLocalSearchParams<{ id: string }>();
  const sessionId = String(id);
  const {
    thread,
    loading,
    error,
    send,
    steer,
    cancel,
    allow,
    deny,
    answer,
    dismissError,
    reportError,
  } = useSession(sessionId);
  const insets = useSafeAreaInsets();
  // The stack header sits above this view, so the keyboard has to be offset past
  // it or it pushes the composer up behind the transcript. Derived from the top
  // inset plus the standard iOS nav-bar height rather than hardcoded, so a
  // notch/Dynamic Island device does not get a phone-sized guess.
  const keyboardOffset = insets.top + 44;

  return (
    <ThemedView style={styles.fill}>
      <Stack.Screen
        options={{
          title: thread.title ?? 'Session',
          headerRight: () => (
            <Link href={{ pathname: '/git/[id]', params: { id: sessionId } }}>
              <ThemedText type="code">Git</ThemedText>
            </Link>
          ),
        }}
      />

      <KeyboardAvoidingView
        style={styles.fill}
        behavior={Platform.OS === 'ios' ? 'padding' : undefined}
        keyboardVerticalOffset={Platform.OS === 'ios' ? keyboardOffset : 0}
      >
        <SafeAreaView style={styles.fill} edges={['bottom']}>
          <StatusStrip thread={thread} />

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
            <Transcript thread={thread} onAllow={allow} onDeny={deny} onAnswer={answer} />
          )}

          <Composer
            turnActive={thread.turn_active}
            onSend={send}
            onSteer={steer}
            onCancel={cancel}
            onError={reportError}
          />
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
