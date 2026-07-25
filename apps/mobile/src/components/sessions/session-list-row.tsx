import { router } from 'expo-router';
import type { SessionSummary } from 'oximux-core';
import { Pressable, StyleSheet, View } from 'react-native';
import Animated, { FadeIn, FadeOut } from 'react-native-reanimated';

import { PROJECT_INDENT } from '@/components/sessions/project-group-header';
import { ThemedText } from '@/components/themed-text';
import { MOTION_DURATION } from '@/constants/motion';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { tick } from '@/native/haptics';
import { parseSessionTitle } from '@/native/session-title';

type Props = {
  session: SessionSummary;
  /** True only in the flat, header-less list (no projects, or while filtering),
   *  where the project has to travel with the row instead of sitting above it. */
  showProject: boolean;
};

/**
 * One session in the list.
 *
 * Indented under its project rather than ruled off from its neighbours. The
 * hairline between every row was inset 24pt on the left and flush to the screen
 * on the right, so each rule pointed off the edge; and being drawn only between
 * rows of the same project, it fell at intervals that tracked group size rather
 * than anything the eye could read. Alignment carries the grouping now.
 */
export function SessionListRow({ session, showProject }: Props) {
  const theme = useTheme();
  // The host folds the project into the title so a row is attributable to its
  // project without a wire-schema change; strip it back off when a project
  // header is already saying it.
  const { project, label } = parseSessionTitle(session.title);
  return (
    <Animated.View
      entering={FadeIn.duration(MOTION_DURATION)}
      exiting={FadeOut.duration(MOTION_DURATION)}
    >
      <Pressable
        onPress={() => {
          tick();
          router.push({ pathname: '/session/[id]', params: { id: session.sessionId } });
        }}
        accessibilityRole="button"
        style={({ pressed }) => [
          styles.row,
          !showProject && styles.indented,
          pressed && { backgroundColor: theme.surface2 },
        ]}
      >
        <View style={styles.text}>
          {showProject && project ? (
            <ThemedText type="small" themeColor="textSecondary" numberOfLines={1}>
              {project}
            </ThemedText>
          ) : null}
          <ThemedText numberOfLines={1}>{label}</ThemedText>
          {session.model ? (
            <ThemedText type="small" themeColor="textMuted" numberOfLines={1}>
              {session.model}
            </ThemedText>
          ) : null}
        </View>
        {/* A session blocked on a permission is the one thing worth crossing the
            room for, so it gets the only accent in the row. */}
        {session.awaitingPermission ? (
          <View style={[styles.attention, { backgroundColor: theme.warning }]} />
        ) : null}
      </Pressable>
    </Animated.View>
  );
}

const styles = StyleSheet.create({
  row: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: Spacing.three,
    paddingVertical: Spacing.two,
    paddingHorizontal: Spacing.four,
  },
  // Left padding only: the row still highlights edge to edge when pressed, which
  // is what makes it feel like a target rather than a floating line of text.
  indented: { paddingLeft: Spacing.four + PROJECT_INDENT },
  text: { flex: 1, gap: Spacing.half },
  attention: { width: 8, height: 8, borderRadius: 4 },
});
