import { useCallback, useEffect, useRef } from 'react';
import { FlatList, StyleSheet, View } from 'react-native';

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
import { Spacing } from '@/constants/theme';
import {
  isAssistant,
  isCompaction,
  isToolCall,
  isTurnDiff,
  isUser,
  pendingPermission,
  pendingQuestion,
  type AskQuestion,
  type QuestionAnswers,
  type Thread,
  type ThreadEntry,
} from '@/native/thread';

type Props = {
  thread: Thread;
  onAllow: (requestId: string, toolInput: unknown) => Promise<unknown>;
  onDeny: (requestId: string, message: string) => Promise<unknown>;
  onAnswer: (
    requestId: string,
    questions: AskQuestion[],
    answers: QuestionAnswers
  ) => Promise<unknown>;
};

export function Transcript({ thread, onAllow, onDeny, onAnswer }: Props) {
  const listRef = useRef<FlatList<ThreadEntry>>(null);
  const count = thread.entries.length;
  // The last entry also *grows* while a reply streams, so the length alone is
  // not enough to know something changed — the streamed text is what moves the
  // bottom of the list.
  const tailLength = lastEntryLength(thread.entries);

  useEffect(() => {
    if (count === 0) return;
    // `scrollToEnd` on an offscreen list is a no-op, so it is deferred a frame.
    const id = setTimeout(() => listRef.current?.scrollToEnd({ animated: true }), 50);
    return () => clearTimeout(id);
  }, [count, tailLength]);

  const renderItem = useCallback(
    ({ item }: { item: ThreadEntry }) => (
      <Entry entry={item} onAllow={onAllow} onDeny={onDeny} onAnswer={onAnswer} />
    ),
    [onAllow, onDeny, onAnswer]
  );

  return (
    <FlatList
      ref={listRef}
      data={thread.entries}
      // Entries have no stable id of their own; index is correct here because
      // the fold only ever appends or mutates in place — it never reorders.
      keyExtractor={(_, index) => String(index)}
      renderItem={renderItem}
      contentContainerStyle={styles.list}
      ListEmptyComponent={
        <ThemedText type="small" style={styles.empty}>
          No messages yet. Send one below.
        </ThemedText>
      }
    />
  );
}

function Entry({
  entry,
  onAllow,
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
          <PermissionCard call={call} request={permission} onAllow={onAllow} onDeny={onDeny} />
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

/**
 * The length of whatever text the last entry carries, so a streaming reply
 * re-triggers the scroll effect as it grows.
 */
function lastEntryLength(entries: ThreadEntry[]): number {
  const last = entries[entries.length - 1];
  if (!last) return 0;
  if (isAssistant(last)) return last.Assistant.text.length + last.Assistant.thinking.length;
  if (isToolCall(last)) return (last.ToolCall.result ?? '').length;
  if (isUser(last)) return last.User.text.length;
  return 0;
}

const styles = StyleSheet.create({
  list: { padding: Spacing.three, gap: Spacing.three },
  empty: { textAlign: 'center', paddingTop: Spacing.five, opacity: 0.7 },
  unknown: { opacity: 0.6 },
});
