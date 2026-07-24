import { Link, Stack, useLocalSearchParams } from 'expo-router';
import { GitBranch, GitPullRequest, History, PanelLeft } from 'lucide-react-native';
import { useState } from 'react';
import { ActivityIndicator, Pressable, StyleSheet, View } from 'react-native';
import { GestureDetector } from 'react-native-gesture-handler';
import { KeyboardAvoidingView } from 'react-native-keyboard-controller';
import { SafeAreaView, useSafeAreaInsets } from 'react-native-safe-area-context';

import { Composer } from '@/components/chat/composer';
import { RewindSheet } from '@/components/chat/rewind-sheet';
import { SessionControls } from '@/components/chat/session-controls';
import { StatusStrip } from '@/components/chat/status-strip';
import { Transcript } from '@/components/chat/transcript';
import { TurnTimer } from '@/components/chat/turn-timer';
import { ConnectionBanner } from '@/components/connection-banner';
import { useAppDrawer } from '@/components/deck/app-drawer';
import { ThemedText } from '@/components/themed-text';
import { ThemedView } from '@/components/themed-view';
import { ErrorBanner } from '@/components/ui/error-banner';
import { Icon } from '@/components/ui/icon';
import { IconButton } from '@/components/ui/icon-button';
import { Spacing } from '@/constants/theme';
import { useSession } from '@/native/session';

/**
 * The core screen: one agent session, live. The transcript arrives already
 * folded from the Rust core, so this composes panels rather than reducing
 * events.
 */
export default function SessionScreen() {
  // `draft` arrives when the forge screen attaches a PR/issue: it routes back
  // here with the composed text as a param rather than through shared state, so
  // the attachment is carried by the navigation that caused it.
  const { id, draft } = useLocalSearchParams<{ id: string; draft?: string }>();
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
  const drawer = useAppDrawer();

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
          // The left edge belongs to the sessions drawer, so the stack's own
          // iOS swipe-back is disabled here; the header back button still returns.
          gestureEnabled: false,
          headerRight: () => (
            <View style={styles.headerActions}>
              <IconButton icon={PanelLeft} accessibilityLabel="Switch session" onPress={drawer.open} />
              <Pressable
                onPress={() => setRewindOpen(true)}
                hitSlop={Spacing.two}
                style={styles.headerAction}
              >
                <Icon icon={History} size="sm" />
                <ThemedText type="code">Rewind</ThemedText>
              </Pressable>
              <Link href={{ pathname: '/forge/[id]', params: { id: sessionId } }} asChild>
                <Pressable style={styles.headerAction}>
                  <Icon icon={GitPullRequest} size="sm" />
                  <ThemedText type="code">PRs</ThemedText>
                </Pressable>
              </Link>
              <Link href={{ pathname: '/git/[id]', params: { id: sessionId } }} asChild>
                <Pressable style={styles.headerAction}>
                  <Icon icon={GitBranch} size="sm" />
                  <ThemedText type="code">Git</ThemedText>
                </Pressable>
              </Link>
            </View>
          ),
        }}
      />

      <GestureDetector gesture={drawer.edgePan}>
        <KeyboardAvoidingView style={styles.fill} behavior="padding" keyboardVerticalOffset={keyboardOffset}>
        <SafeAreaView style={styles.fill} edges={['bottom']}>
          <ConnectionBanner />
          <StatusStrip thread={thread} />
          <TurnTimer active={thread.turn_active} />

          {error ? <ErrorBanner message={error} onDismiss={dismissError} /> : null}

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
            draft={draft}
            onSend={send}
            onSteer={steer}
            onCancel={cancel}
            onError={reportError}
          />
        </SafeAreaView>
        </KeyboardAvoidingView>
      </GestureDetector>

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
  headerActions: { flexDirection: 'row', alignItems: 'center', gap: Spacing.three },
  headerAction: { flexDirection: 'row', alignItems: 'center', gap: Spacing.one },
});
