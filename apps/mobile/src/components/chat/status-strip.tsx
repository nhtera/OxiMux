import { useState } from 'react';
import { Pressable, StyleSheet, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { ErrorBanner } from '@/components/ui/error-banner';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import type { PlanEntryLite, Thread } from '@/native/thread';

/**
 * The pinned header: the agent's plan (when it has one) and the turn's usage.
 * Both are collapsed by default — on a phone the transcript is the scarce
 * resource, and neither is worth permanent vertical space.
 */
export function StatusStrip({ thread }: { thread: Thread }) {
  const theme = useTheme();
  const [openPlan, setOpenPlan] = useState(false);
  const plan = thread.plan ?? [];
  const done = plan.filter((e) => e.status === 'completed').length;

  const usage = thread.live_usage ?? thread.usage;
  const window = thread.context_window ?? usage?.context_window ?? null;

  if (plan.length === 0 && !usage && !thread.compacting && !thread.last_error) return null;

  return (
    <View style={[styles.strip, { borderBottomColor: theme.backgroundSelected }]}>
      {thread.last_error ? <ErrorBanner message={thread.last_error} /> : null}

      {thread.compacting ? (
        <ThemedText type="small" style={styles.muted}>
          Compacting context…
        </ThemedText>
      ) : null}

      {plan.length > 0 ? (
        <Pressable onPress={() => setOpenPlan((v) => !v)} hitSlop={Spacing.two}>
          <ThemedText type="small">
            {openPlan ? '▾' : '▸'} Plan · {done}/{plan.length}
          </ThemedText>
        </Pressable>
      ) : null}

      {openPlan
        ? plan.map((entry, i) => <PlanRow key={`${i}-${entry.content}`} entry={entry} />)
        : null}

      {usage ? (
        <ThemedText type="small" style={styles.muted}>
          {formatTokens(usage.input_tokens + usage.output_tokens)} tokens
          {window ? ` · ${percentOf(usage.input_tokens, window)}% of context` : ''}
          {thread.session_cost_usd > 0
            ? ` · $${thread.session_cost_usd.toFixed(2)} this session`
            : ''}
        </ThemedText>
      ) : null}
    </View>
  );
}

function PlanRow({ entry }: { entry: PlanEntryLite }) {
  const mark =
    entry.status === 'completed' ? '✓' : entry.status === 'in_progress' ? '▸' : '○';
  return (
    <ThemedText
      type="small"
      numberOfLines={2}
      style={[styles.planRow, entry.status === 'completed' && styles.muted]}
    >
      {mark} {entry.content}
    </ThemedText>
  );
}

function formatTokens(n: number): string {
  return n >= 1000 ? `${(n / 1000).toFixed(1)}k` : String(n);
}

function percentOf(used: number, window: number): number {
  return window > 0 ? Math.min(100, Math.round((used / window) * 100)) : 0;
}

const styles = StyleSheet.create({
  strip: {
    borderBottomWidth: StyleSheet.hairlineWidth,
    paddingHorizontal: Spacing.three,
    paddingVertical: Spacing.two,
    gap: Spacing.one,
  },
  planRow: { paddingLeft: Spacing.two },
  muted: { opacity: 0.7 },
});
