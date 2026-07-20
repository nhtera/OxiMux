import { useEffect, useMemo, useRef, useState } from 'react';
import { ActivityIndicator, Pressable, StyleSheet, View } from 'react-native';

import { QuestionBlock } from '@/components/chat/question-block';
import { ThemedText } from '@/components/themed-text';
import { QUESTION_ACCENT as ACCENT } from '@/constants/chat';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import {
  allAnswered,
  type AskQuestion,
  type QuestionAnswer,
  type QuestionAnswers,
  type QuestionRequest,
  type ToolCall,
} from '@/native/thread';

type Props = {
  call: ToolCall;
  request: QuestionRequest;
  // Loose return type: the card only needs the send to settle. A failure already
  // surfaces on the screen's error row.
  onAnswer: (requestId: string, questions: AskQuestion[], answers: QuestionAnswers) => Promise<unknown>;
};

/**
 * The interactive card for an `AskUserQuestion`. Rendered inline like the
 * permission card, and for the same reason: the surrounding transcript is the
 * context needed to choose.
 *
 * A question marked `is_secret` is deliberately NOT answerable here. The desktop
 * redacts a secret answer by flagging the tool call as the answer is sent — the
 * last moment the secret-ness is knowable — and that flag lives on the desktop's
 * own thread, which the remote path cannot reach. Answering one from the phone
 * would leave the credential in the persisted transcript in plain text, so this
 * points at the desktop instead. The core refuses it too; this is the half that
 * keeps the user from trying.
 */
export function QuestionCard({ call, request, onAnswer }: Props) {
  const theme = useTheme();
  const [answers, setAnswers] = useState<QuestionAnswers>({ by_question: {}, response: null });
  const [busy, setBusy] = useState(false);
  // Submitting is what dismisses this card: the tool call leaves AwaitingAnswer
  // and the transcript re-renders without it, which can land before the send
  // promise settles.
  const mounted = useRef(true);
  useEffect(
    () => () => {
      mounted.current = false;
    },
    []
  );

  const secret = useMemo(() => request.questions.some((q) => q.is_secret), [request.questions]);
  const ready = allAnswered(request.questions, answers);

  const update = (id: string, patch: Partial<QuestionAnswer>) =>
    setAnswers((prev) => {
      // A question untouched so far has no entry yet, so the blank draft is the
      // base the patch lands on.
      const prior: QuestionAnswer = prev.by_question[id] ?? { selected: [], custom: null };
      return {
        ...prev,
        by_question: { ...prev.by_question, [id]: { ...prior, ...patch } },
      };
    });

  const submit = async (payload: QuestionAnswers) => {
    if (busy) return;
    setBusy(true);
    try {
      await onAnswer(request.request_id, request.questions, payload);
    } finally {
      if (mounted.current) setBusy(false);
    }
  };

  if (secret) {
    return (
      <View style={[styles.card, { borderColor: ACCENT, backgroundColor: theme.backgroundElement }]}>
        <ThemedText type="small" style={styles.kicker}>
          Question · {call.name}
        </ThemedText>
        <ThemedText style={styles.body}>{request.questions[0]?.question}</ThemedText>
        <ThemedText type="small" style={styles.note}>
          This asks for a secret, so it can only be answered on the desktop — an answer sent from
          here would be stored unredacted. The turn stays paused until you do.
        </ThemedText>
      </View>
    );
  }

  return (
    <View style={[styles.card, { borderColor: ACCENT, backgroundColor: theme.backgroundElement }]}>
      <ThemedText type="small" style={styles.kicker}>
        Question · {call.name}
      </ThemedText>

      {request.questions.map((question) => (
        <QuestionBlock
          key={question.id}
          question={question}
          answer={answers.by_question[question.id]}
          disabled={busy}
          onChange={(patch) => update(question.id, patch)}
        />
      ))}

      <View style={styles.actions}>
        <Pressable
          disabled={!ready || busy}
          onPress={() => submit(answers)}
          style={[styles.button, { backgroundColor: ACCENT }, (!ready || busy) && styles.dim]}
        >
          <ThemedText type="code" style={styles.submitLabel}>
            Submit
          </ThemedText>
        </Pressable>

        {/* Skip sends an empty answer set, which the tool reads as "did not
            answer" — the same thing the desktop's Skip does. It releases the
            turn rather than leaving the agent blocked. */}
        <Pressable
          disabled={busy}
          onPress={() => submit({ by_question: {}, response: null })}
          style={[styles.button, styles.skip, busy && styles.dim]}
        >
          <ThemedText type="code">Skip</ThemedText>
        </Pressable>

        {busy ? <ActivityIndicator /> : null}
      </View>
    </View>
  );
}

const styles = StyleSheet.create({
  card: {
    borderWidth: 1,
    borderRadius: Spacing.two,
    padding: Spacing.three,
    gap: Spacing.two,
  },
  kicker: { color: ACCENT },
  body: { lineHeight: 20 },
  actions: { flexDirection: 'row', alignItems: 'center', gap: Spacing.two, paddingTop: Spacing.one },
  button: {
    paddingVertical: Spacing.two,
    paddingHorizontal: Spacing.three,
    borderRadius: Spacing.two,
  },
  skip: { borderWidth: 1, borderColor: '#8A8F98' },
  submitLabel: { color: '#000000' },
  dim: { opacity: 0.4 },
  note: { opacity: 0.8 },
});
