import { useEffect, useRef, useState } from 'react';
import { ActivityIndicator, Pressable, StyleSheet, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import type { PermissionRequest, QuestionRequest, ToolCall } from '@/native/thread';

const ACCENT = '#F5A623';

type Props = {
  call: ToolCall;
  request: PermissionRequest;
  // Return type is deliberately loose: the card only needs these to settle, not
  // to report an outcome — a failure already surfaces on the screen's error row.
  onAllow: (requestId: string, toolInput: unknown) => Promise<unknown>;
  onDeny: (requestId: string, message: string) => Promise<unknown>;
};

/**
 * The approve/deny card for a blocked tool call — the highest-leverage thing the
 * phone does, so it is rendered inline in the transcript rather than as a
 * blocking modal: the surrounding conversation is exactly the context needed to
 * decide, and a modal would hide it.
 *
 * A `plan` request carries the plan markdown in `description` rather than a file
 * name, so it gets the full body instead of a one-line summary.
 */
export function PermissionCard({ call, request, onAllow, onDeny }: Props) {
  const theme = useTheme();
  const [busy, setBusy] = useState(false);
  /**
   * Resolving is what makes this card disappear: the tool call leaves
   * `WaitingForConfirmation` and the transcript re-renders without it. That
   * snapshot can land before the decision promise settles, so the cleanup below
   * would otherwise set state on an unmounted component.
   */
  const mounted = useRef(true);
  useEffect(
    () => () => {
      mounted.current = false;
    },
    []
  );

  // The buttons stay disabled once tapped. The host is idempotent (a second
  // resolve of the same request is refused, not double-applied), so this is
  // about not leaving the user staring at an unchanged card, not about safety.
  const decide = async (decision: () => Promise<unknown>) => {
    if (busy) return;
    setBusy(true);
    try {
      await decision();
    } finally {
      if (mounted.current) setBusy(false);
    }
  };

  const isPlan = request.kind === 'plan';

  return (
    <View style={[styles.card, { borderColor: ACCENT, backgroundColor: theme.backgroundElement }]}>
      <ThemedText type="small" style={styles.kicker}>
        {label(request.kind)} · {call.name}
      </ThemedText>

      <ThemedText type={isPlan ? 'default' : 'code'} style={styles.body}>
        {request.description}
      </ThemedText>

      <View style={styles.actions}>
        <Pressable
          disabled={busy}
          onPress={() => decide(() => onAllow(request.request_id, call.input))}
          style={[styles.button, { backgroundColor: ACCENT }, busy && styles.busy]}
        >
          <ThemedText type="code" style={styles.allowLabel}>
            {isPlan ? 'Approve plan' : 'Allow'}
          </ThemedText>
        </Pressable>

        <Pressable
          disabled={busy}
          onPress={() =>
            decide(() => onDeny(request.request_id, 'Denied from the OxiMux mobile app.'))
          }
          style={[styles.button, styles.deny, busy && styles.busy]}
        >
          <ThemedText type="code">Deny</ThemedText>
        </Pressable>

        {busy ? <ActivityIndicator /> : null}
      </View>

      {/* Agent-offered shortcuts ("always allow this pattern") are shown so the
          user knows the desktop can widen the grant, but they are not actionable
          here: applying one needs the suggestion's opaque payload echoed back on
          a decision variant this FFI does not expose yet. Listing them without
          wiring them would promise a button that does nothing. */}
      {request.suggestions.length > 0 ? (
        <ThemedText type="small" style={styles.note}>
          {request.suggestions.length === 1 ? 'A shortcut is' : `${request.suggestions.length} shortcuts are`}{' '}
          offered on the desktop for this request.
        </ThemedText>
      ) : null}
    </View>
  );
}

/**
 * Questions block a turn exactly as permissions do, but answering one needs a
 * selection payload the FFI does not carry yet, so this states plainly where it
 * can be answered rather than rendering dead option rows.
 */
export function QuestionCard({ call, request }: { call: ToolCall; request: QuestionRequest }) {
  const theme = useTheme();
  const first = request.questions[0];
  return (
    <View style={[styles.card, { borderColor: ACCENT, backgroundColor: theme.backgroundElement }]}>
      <ThemedText type="small" style={styles.kicker}>
        Question · {call.name}
      </ThemedText>
      {first ? <ThemedText style={styles.body}>{first.question}</ThemedText> : null}
      <ThemedText type="small" style={styles.note}>
        Answer this on the desktop — the turn stays paused until you do.
      </ThemedText>
    </View>
  );
}

function label(kind: PermissionRequest['kind']): string {
  switch (kind) {
    case 'plan':
      return 'Plan approval';
    case 'mode':
      return 'Mode change';
    case 'mcp':
      return 'MCP request';
    case 'tool':
    case 'other':
    default:
      return 'Permission';
  }
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
  deny: { borderWidth: 1, borderColor: '#8A8F98' },
  allowLabel: { color: '#000000' },
  busy: { opacity: 0.5 },
  note: { opacity: 0.8 },
});
