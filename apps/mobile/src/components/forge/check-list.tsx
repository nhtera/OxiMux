import { StyleSheet, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { checkStatus, checksSummary, type CheckStatus } from '@/native/forge';
import type { CheckRun } from 'oximux-core';

const GLYPH: Record<CheckStatus, string> = {
  pass: '✓',
  fail: '✕',
  pending: '•',
  skipped: '–',
  unknown: '?',
};

function colorFor(status: CheckStatus, theme: ReturnType<typeof useTheme>): string {
  switch (status) {
    case 'pass':
      return theme.success;
    case 'fail':
      return theme.danger;
    case 'pending':
      return theme.info;
    case 'skipped':
    case 'unknown':
      return theme.textMuted;
  }
}

/**
 * CI checks for the current branch's PR.
 *
 * Renders **nothing at all** when there are no checks, rather than an empty
 * panel. No checks is indistinguishable from no PR, no CI configured, or a
 * GitLab repo (which reports none by design) — so a header promising check
 * status above blank space would imply something is missing when nothing is.
 */
export function CheckList({ checks }: { checks: CheckRun[] }) {
  const theme = useTheme();
  if (checks.length === 0) return null;

  return (
    <View style={[styles.panel, { backgroundColor: theme.backgroundElement }]}>
      <ThemedText type="smallBold">Checks</ThemedText>
      <ThemedText type="small" style={styles.summary}>
        {checksSummary(checks)}
      </ThemedText>
      {checks.map((check) => {
        const status = checkStatus(check);
        return (
          <View key={`${check.name}-${check.link}`} style={styles.row}>
            <ThemedText type="code" style={{ color: colorFor(status, theme) }}>
              {GLYPH[status]}
            </ThemedText>
            <ThemedText type="small" numberOfLines={1} style={styles.name}>
              {check.name}
            </ThemedText>
            {check.description ? (
              <ThemedText type="small" style={styles.summary} numberOfLines={1}>
                {check.description}
              </ThemedText>
            ) : null}
          </View>
        );
      })}
    </View>
  );
}

const styles = StyleSheet.create({
  panel: {
    margin: Spacing.three,
    padding: Spacing.three,
    borderRadius: Spacing.two,
    gap: Spacing.one,
  },
  row: { flexDirection: 'row', alignItems: 'center', gap: Spacing.two },
  name: { flex: 1 },
  summary: { opacity: 0.7 },
});
