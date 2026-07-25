import { ChevronDown } from 'lucide-react-native';
import { useCallback, useRef, useState } from 'react';
import {
  FlatList,
  type NativeScrollEvent,
  type NativeSyntheticEvent,
  StyleSheet,
  View,
} from 'react-native';
import Animated, { FadeIn, FadeOut } from 'react-native-reanimated';

import {
  AssistantBubble,
  CompactionDivider,
  ToolCallCard,
  TurnDiffCard,
  UserBubble,
} from '@/components/chat/entries';
import { PermissionCard } from '@/components/chat/permission-card';
import { QuestionCard } from '@/components/chat/question-card';
import { ThemedText } from '@/components/themed-text';
import { IconButton } from '@/components/ui/icon-button';
import { Radius, Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import {
  isAssistant,
  isCompaction,
  isToolCall,
  isTurnDiff,
  isUser,
  pendingPermission,
  pendingQuestion,
  type AskQuestion,
  type PermissionSuggestion,
  type QuestionAnswers,
  type Thread,
  type ThreadEntry,
} from '@/native/thread';

type Props = {
  thread: Thread;
  onAllow: (requestId: string, toolInput: unknown) => Promise<unknown>;
  onAllowWith: (
    requestId: string,
    toolInput: unknown,
    suggestion: PermissionSuggestion
  ) => Promise<unknown>;
  onDeny: (requestId: string, message: string) => Promise<unknown>;
  onAnswer: (
    requestId: string,
    questions: AskQuestion[],
    answers: QuestionAnswers
  ) => Promise<unknown>;
};

/** How close to the bottom still counts as "following the conversation". */
const NEAR_BOTTOM_PX = 80;

export function Transcript({ thread, onAllow, onAllowWith, onDeny, onAnswer }: Props) {
  const theme = useTheme();
  const listRef = useRef<FlatList<ThreadEntry>>(null);
  // Whether the user is pinned to the bottom. A ref, not state, so a streaming
  // reply firing `onContentSizeChange` many times a second never re-renders the
  // whole transcript — only the pill's visibility (below) is React state.
  const atBottom = useRef(true);
  const [showPill, setShowPill] = useState(false);

  const toEnd = useCallback((animated: boolean) => {
    listRef.current?.scrollToEnd({ animated });
    atBottom.current = true;
    setShowPill(false);
  }, []);

  const onScroll = useCallback((e: NativeSyntheticEvent<NativeScrollEvent>) => {
    const { contentOffset, contentSize, layoutMeasurement } = e.nativeEvent;
    const distance = contentSize.height - (contentOffset.y + layoutMeasurement.height);
    const near = distance < NEAR_BOTTOM_PX;
    atBottom.current = near;
    // Only flip React state on an actual change — not on every scroll frame.
    setShowPill((shown) => (shown === near ? !near : shown));
  }, []);

  // Stick to the bottom as content grows (new entry or a streaming reply) only
  // when the user was already there. Someone reading history stays put — the
  // old unconditional `scrollToEnd` yanked them down on every token.
  const onContentSizeChange = useCallback(() => {
    if (atBottom.current) listRef.current?.scrollToEnd({ animated: true });
  }, []);

  const renderItem = useCallback(
    ({ item }: { item: ThreadEntry }) => (
      <Entry
        entry={item}
        onAllow={onAllow}
        onAllowWith={onAllowWith}
        onDeny={onDeny}
        onAnswer={onAnswer}
      />
    ),
    [onAllow, onAllowWith, onDeny, onAnswer]
  );

  return (
    <View style={styles.fill}>
      <FlatList
        ref={listRef}
        data={thread.entries}
        // Entries have no stable id of their own; index is correct here because
        // the fold only ever appends or mutates in place — it never reorders.
        keyExtractor={(_, index) => String(index)}
        renderItem={renderItem}
        contentContainerStyle={styles.list}
        onScroll={onScroll}
        scrollEventThrottle={32}
        onContentSizeChange={onContentSizeChange}
        ListEmptyComponent={
          <ThemedText type="small" style={styles.empty}>
            No messages yet. Send one below.
          </ThemedText>
        }
      />
      {showPill ? (
        <Animated.View entering={FadeIn.duration(150)} exiting={FadeOut.duration(150)} style={styles.pillWrap}>
          <IconButton
            icon={ChevronDown}
            accessibilityLabel="Scroll to latest message"
            onPress={() => toEnd(true)}
            color={theme.text}
            style={[styles.pill, { backgroundColor: theme.surface2, borderColor: theme.border }]}
          />
        </Animated.View>
      ) : null}
    </View>
  );
}

function Entry({
  entry,
  onAllow,
  onAllowWith,
  onDeny,
  onAnswer,
}: { entry: ThreadEntry } & Omit<Props, 'thread'>) {
  if (isUser(entry)) {
    return <UserBubble text={entry.User.text} images={entry.User.images} />;
  }
  if (isAssistant(entry)) {
    return <AssistantBubble message={entry.Assistant} />;
  }
  if (isCompaction(entry)) {
    return <CompactionDivider summary={entry.ContextCompaction.summary} />;
  }
  if (isTurnDiff(entry)) {
    return <TurnDiffCard files={entry.TurnDiff.files} diff={entry.TurnDiff.diff} />;
  }
  if (isToolCall(entry)) {
    const call = entry.ToolCall;
    const permission = pendingPermission(call);
    const question = pendingQuestion(call);
    return (
      <ToolCallCard call={call}>
        {permission ? (
          <PermissionCard
            call={call}
            request={permission}
            onAllow={onAllow}
            onAllowWith={onAllowWith}
            onDeny={onDeny}
          />
        ) : null}
        {question ? (
          <QuestionCard call={call} request={question} onAnswer={onAnswer} />
        ) : null}
      </ToolCallCard>
    );
  }
  // An entry variant this build does not know: a desktop newer than the app.
  // Rendering nothing would silently swallow it, so it is named instead.
  return (
    <View style={styles.unknown}>
      <ThemedText type="small">An entry this app version cannot display.</ThemedText>
    </View>
  );
}

const styles = StyleSheet.create({
  fill: { flex: 1 },
  list: { padding: Spacing.three, gap: Spacing.three },
  empty: { textAlign: 'center', paddingTop: Spacing.five, opacity: 0.7 },
  unknown: { opacity: 0.6 },
  pillWrap: { position: 'absolute', right: Spacing.three, bottom: Spacing.three },
  pill: {
    width: 40,
    height: 40,
    borderRadius: Radius.full,
    borderWidth: StyleSheet.hairlineWidth,
  },
});
