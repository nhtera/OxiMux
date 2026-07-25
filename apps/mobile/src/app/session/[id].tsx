import { Link, Stack, router, useLocalSearchParams } from 'expo-router';
import { Archive, GitBranch, GitPullRequest, History, PanelLeft } from 'lucide-react-native';
import { useState } from 'react';
import { ActivityIndicator, Pressable, StyleSheet, View } from 'react-native';
import { GestureDetector } from 'react-native-gesture-handler';
import { KeyboardAvoidingView } from 'react-native-keyboard-controller';
import { SafeAreaView, useSafeAreaInsets } from 'react-native-safe-area-context';

import { Composer } from '@/components/chat/composer';
import { RewindSheet } from '@/components/chat/rewind-sheet';
import { SessionClosedNotice } from '@/components/chat/session-closed-notice';
import { useSessionControls } from '@/components/chat/session-controls';
import { StatusStrip } from '@/components/chat/status-strip';
import { Transcript } from '@/components/chat/transcript';
import { TurnTimer } from '@/components/chat/turn-timer';
import { ConnectionBanner } from '@/components/connection-banner';
import { useAppDrawer } from '@/components/deck/app-drawer';
import { ThemedText } from '@/components/themed-text';
import { ThemedView } from '@/components/themed-view';
import { EmptyState } from '@/components/ui/empty-state';
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
    closed,
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
  // Chips render inside the composer's control row; the pickers they open are
  // full-width sheets and are rendered at the screen root, beside the rewind
  // sheet, so they anchor to the window rather than to that row.
  const sessionControls = useSessionControls(sessionId, reportError);
  const [rewindOpen, setRewindOpen] = useState(false);
  const [rewinding, setRewinding] = useState(false);
  const [rewindError, setRewindError] = useState<string>();
  const insets = useSafeAreaInsets();
  const drawer = useAppDrawer();
  // `replace`, not `back`: a session that no longer exists should not stay on the
  // stack for a swipe or a second Back to land on again.
  const toSessions = () => router.replace('/sessions');

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
              {/* Rewind, PRs and Git all address this session on the host, so a
                  closed one leaves them as buttons that can only fail. Switching
                  session is the one action that still means something. */}
              {closed ? null : (
                <>
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
                </>
              )}
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

          {/* A closed session's own state says everything the banner would, and
              says it without the red — an action that failed because the tab is
              gone is not a failure the user needs to read twice. */}
          {error && !closed ? <ErrorBanner message={error} onDismiss={dismissError} /> : null}

          {loading ? (
            <View style={styles.centre}>
              <ActivityIndicator />
            </View>
          ) : closed && thread.entries.length === 0 ? (
            // Nothing was ever loaded — the session was already gone when this
            // screen opened, so there is no transcript to leave on screen. This
            // state carries the whole message, which is why the notice below
            // stands down: one screen, one explanation.
            <View style={styles.fill}>
              <EmptyState
                icon={Archive}
                title="This session is no longer open"
                message="It was closed on the desktop."
                action={{ label: 'Back to sessions', onPress: toSessions }}
              />
            </View>
          ) : (
            // A transcript that did load stays readable: closing the tab ends the
            // session, it does not make what was said unreadable.
            <Transcript
              thread={thread}
              onAllow={allow}
              onAllowWith={allowWith}
              onDeny={deny}
              onAnswer={answer}
            />
          )}

          {closed ? (
            // Only where a transcript is on screen — otherwise the empty state
            // above has already said it, and said it with room to breathe.
            thread.entries.length > 0 ? <SessionClosedNotice onBack={toSessions} /> : null
          ) : (
            <Composer
              turnActive={thread.turn_active}
              slashCommands={thread.slash_commands}
              controls={sessionControls.chips}
              draft={draft}
              onSend={send}
              onSteer={steer}
              onCancel={cancel}
              onError={reportError}
            />
          )}
        </SafeAreaView>
        </KeyboardAvoidingView>
      </GestureDetector>

      {sessionControls.pickers}

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
