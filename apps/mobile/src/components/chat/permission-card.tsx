import { useEffect, useRef, useState } from 'react';
import { ActivityIndicator, Pressable, StyleSheet, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { QUESTION_ACCENT as ACCENT } from '@/constants/chat';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import type { PermissionRequest, PermissionSuggestion, ToolCall } from '@/native/thread';

type Props = {
  call: ToolCall;
  request: PermissionRequest;
  // Return type is deliberately loose: the card only needs these to settle, not
  // to report an outcome — a failure already surfaces on the screen's error row.
  onAllow: (requestId: string, toolInput: unknown) => Promise<unknown>;
  onDeny: (requestId: string, message: string) => Promise<unknown>;
  /**
   * Allow *and* apply one of the agent's own offered shortcuts. The suggestion
   * is passed back exactly as it arrived — its `raw` payload is opaque to the
   * phone and only the agent that offered it knows what it means.
   */
  onAllowWith?: (
    requestId: string,
    toolInput: unknown,
    suggestion: PermissionSuggestion
  ) => Promise<unknown>;
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
export function PermissionCard({ call, request, onAllow, onDeny, onAllowWith }: Props) {
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

      {/* The agent's own shortcuts — "always allow edits this session" and the
          like. These were listed but inert until the FFI gained a decision
          variant that echoes the suggestion back; they now apply for real.
          Rendered below the plain Allow so the narrower choice stays the
          default: these widen what the agent may do for the rest of the
          session, which is not what an accidental tap should buy. */}
      {onAllowWith && request.suggestions.length > 0
        ? request.suggestions.map((suggestion, i) => (
            <Pressable
              key={`${suggestion.kind}-${i}`}
              disabled={busy}
              onPress={() =>
                decide(() => onAllowWith(request.request_id, call.input, suggestion))
              }
              style={[styles.suggestion, busy && styles.busy]}
            >
              <ThemedText type="code" numberOfLines={2}>
                {suggestion.label}
              </ThemedText>
            </Pressable>
          ))
        : null}

      {/* No handler wired: say what exists rather than showing a dead button. */}
      {!onAllowWith && request.suggestions.length > 0 ? (
        <ThemedText type="small" style={styles.note}>
          {request.suggestions.length === 1 ? 'A shortcut is' : `${request.suggestions.length} shortcuts are`}{' '}
          offered on the desktop for this request.
        </ThemedText>
      ) : null}
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
  suggestion: {
    borderWidth: StyleSheet.hairlineWidth,
    borderColor: ACCENT,
    borderRadius: Spacing.two,
    paddingVertical: Spacing.two,
    paddingHorizontal: Spacing.three,
  },
});
