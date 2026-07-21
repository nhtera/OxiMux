import { Link, Stack, useLocalSearchParams } from 'expo-router';
import { useState } from 'react';
import { ActivityIndicator, KeyboardAvoidingView, Platform, Pressable, StyleSheet, View } from 'react-native';
import { SafeAreaView, useSafeAreaInsets } from 'react-native-safe-area-context';

import { Composer } from '@/components/chat/composer';
import { RewindSheet } from '@/components/chat/rewind-sheet';
import { SessionControls } from '@/components/chat/session-controls';
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
    allowWith,
    deny,
    answer,
    rewind,
    dismissError,
    reportError,
  } = useSession(sessionId);
  const [rewindOpen, setRewindOpen] = useState(false);
  const [rewinding, setRewinding] = useState(false);
  const [rewindError, setRewindError] = useState<string>();
  const insets = useSafeAreaInsets();

  const onRewind = async (ordinal: number) => {
    setRewinding(true);
    setRewindError(undefined);
    const ok = await rewind(ordinal);
    setRewinding(false);
    // Closed only on success. A failure leaves the sheet open holding its
    // error, because the alternative — dismissing and showing the message
    // somewhere else — reads as though the rewind went through.
    if (ok) setRewindOpen(false);
    else setRewindError('The rewind did not go through. The transcript is unchanged.');
  };
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
            <View style={styles.headerActions}>
              <Pressable onPress={() => setRewindOpen(true)} hitSlop={Spacing.two}>
                <ThemedText type="code">Rewind</ThemedText>
              </Pressable>
              <Link href={{ pathname: '/git/[id]', params: { id: sessionId } }}>
                <ThemedText type="code">Git</ThemedText>
              </Link>
            </View>
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
            <Transcript
              thread={thread}
              onAllow={allow}
              onAllowWith={allowWith}
              onDeny={deny}
              onAnswer={answer}
            />
          )}

          <Composer
            turnActive={thread.turn_active}
            slashCommands={thread.slash_commands}
            controls={<SessionControls sessionId={id} />}
            onSend={send}
            onSteer={steer}
            onCancel={cancel}
            onError={reportError}
          />
        </SafeAreaView>
      </KeyboardAvoidingView>

      <RewindSheet
        visible={rewindOpen}
        entries={thread.entries}
        busy={rewinding}
        error={rewindError}
        onRewind={onRewind}
        onClose={() => {
          setRewindOpen(false);
          setRewindError(undefined);
        }}
      />
    </ThemedView>
  );
}

const styles = StyleSheet.create({
  fill: { flex: 1 },
  centre: { flex: 1, alignItems: 'center', justifyContent: 'center' },
  error: { paddingHorizontal: Spacing.three, paddingVertical: Spacing.two },
  errorText: { color: '#F85149' },
  headerActions: { flexDirection: 'row', alignItems: 'center', gap: Spacing.three },
});
