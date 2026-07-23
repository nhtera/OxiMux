import { router } from 'expo-router';
import { Circle } from 'lucide-react-native';
import type { SessionSummary } from 'oximux-core';
import { useCallback, useState } from 'react';
import { Dimensions, StyleSheet, View } from 'react-native';
import { Gesture } from 'react-native-gesture-handler';
import Animated, {
  interpolate,
  runOnJS,
  useAnimatedStyle,
  useSharedValue,
  withSpring,
  type SharedValue,
} from 'react-native-reanimated';

import { ThemedText } from '@/components/themed-text';
import { Icon } from '@/components/ui/icon';
import { ListRow } from '@/components/ui/list-row';
import { Radius, Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { useClient } from '@/native/client';

const SCREEN = Dimensions.get('window').width;
/** Panel width — most of the screen, but never full-bleed so the chat peeks. */
export const DRAWER_WIDTH = Math.min(320, SCREEN * 0.84);
/** Flick past a third of the panel, or fast, to commit open/closed. */
const COMMIT_FRACTION = DRAWER_WIDTH / 3;
const COMMIT_VELOCITY = 500;
/** Left-edge grab zone for opening the drawer from the chat. */
const EDGE_ZONE = 28;
const SPRING = { damping: 20, stiffness: 220 } as const;

/**
 * The swipe-deck's left panel: the sessions list as a drawer over the active
 * chat. A single `progress` value (0 closed → 1 open) drives the panel slide and
 * the backdrop dim together, so a drag never desyncs them. A React `isOpen`
 * mirror gates backdrop touches (reanimated can animate opacity but not
 * pointer-events reliably). The chat screen owns the edge gesture that opens it.
 */
export function useSessionsDrawer() {
  const progress = useSharedValue(0);
  const [isOpen, setIsOpen] = useState(false);

  const settle = useCallback(
    (toOpen: boolean) => {
      setIsOpen(toOpen);
      // Mutating a reanimated shared value is the intended API; the compiler's
      // immutability rule does not model shared values.
      // eslint-disable-next-line react-hooks/immutability
      progress.value = withSpring(toOpen ? 1 : 0, SPRING);
    },
    [progress]
  );
  const open = useCallback(() => settle(true), [settle]);
  const close = useCallback(() => settle(false), [settle]);

  // Left-edge pan on the chat opens; a pan on the open drawer closes. Vertical
  // intent fails the gesture so it never fights the transcript scroll.
  const edgePan = Gesture.Pan()
    .activeOffsetX([-12, 12])
    .failOffsetY([-16, 16])
    .onUpdate((e) => {
      'worklet';
      const fromOpen = progress.value > 0.5;
      // Opening is only from the very left edge, so horizontal scrolls inside the
      // chat (code blocks, diffs) are left alone.
      if (!fromOpen && e.x - e.translationX > EDGE_ZONE) return;
      const base = fromOpen ? DRAWER_WIDTH : 0;
      // eslint-disable-next-line react-hooks/immutability
      progress.value = Math.min(1, Math.max(0, (base + e.translationX) / DRAWER_WIDTH));
    })
    .onEnd((e) => {
      'worklet';
      const openEnough = progress.value * DRAWER_WIDTH > COMMIT_FRACTION || e.velocityX > COMMIT_VELOCITY;
      const closeFast = e.velocityX < -COMMIT_VELOCITY;
      runOnJS(settle)(openEnough && !closeFast);
    });

  return { progress, isOpen, open, close, edgePan };
}

export function SessionsDrawer({
  progress,
  isOpen,
  close,
  activeId,
}: {
  progress: SharedValue<number>;
  isOpen: boolean;
  close: () => void;
  activeId: string;
}) {
  const theme = useTheme();
  const sessions = useClient((s) => s.sessions);

  const go = useCallback(
    (id: string) => {
      close();
      if (id !== activeId) router.replace({ pathname: '/session/[id]', params: { id } });
    },
    [close, activeId]
  );

  const backdropStyle = useAnimatedStyle(() => ({ opacity: interpolate(progress.value, [0, 1], [0, 0.5]) }));
  const panelStyle = useAnimatedStyle(() => ({
    transform: [{ translateX: interpolate(progress.value, [0, 1], [-DRAWER_WIDTH, 0]) }],
  }));

  return (
    <>
      <Animated.View
        pointerEvents={isOpen ? 'auto' : 'none'}
        onTouchEnd={close}
        style={[styles.backdrop, backdropStyle]}
      />
      <Animated.View
        pointerEvents={isOpen ? 'auto' : 'none'}
        style={[styles.panel, { backgroundColor: theme.surface1, borderRightColor: theme.border }, panelStyle]}
      >
        <View style={styles.panelInner}>
          <ThemedText type="smallBold" style={styles.title}>
            Sessions
          </ThemedText>
          {sessions.map((s: SessionSummary) => (
            <ListRow key={s.sessionId} onPress={() => go(s.sessionId)}>
              <View style={styles.rowHead}>
                {s.sessionId === activeId ? <Icon icon={Circle} size="xs" color={theme.accent} /> : null}
                <ThemedText numberOfLines={1} style={styles.rowTitle}>
                  {s.title}
                </ThemedText>
                {s.awaitingPermission ? <View style={[styles.dot, { backgroundColor: theme.warning }]} /> : null}
              </View>
              {s.model ? (
                <ThemedText type="small" numberOfLines={1} style={{ color: theme.textSecondary }}>
                  {s.model}
                </ThemedText>
              ) : null}
            </ListRow>
          ))}
        </View>
      </Animated.View>
    </>
  );
}

const styles = StyleSheet.create({
  // A plain black scrim — a dim overlay reads the same in light and dark, so it
  // stays a fixed colour rather than a theme token.
  backdrop: { position: 'absolute', top: 0, left: 0, right: 0, bottom: 0, backgroundColor: '#000000', zIndex: 1 },
  panel: {
    position: 'absolute',
    top: 0,
    bottom: 0,
    left: 0,
    width: DRAWER_WIDTH,
    borderRightWidth: StyleSheet.hairlineWidth,
    borderTopRightRadius: Radius.lg,
    borderBottomRightRadius: Radius.lg,
    zIndex: 2,
  },
  panelInner: { flex: 1, paddingHorizontal: Spacing.three, paddingTop: Spacing.four, gap: Spacing.one },
  title: { paddingBottom: Spacing.two },
  rowHead: { flexDirection: 'row', alignItems: 'center', gap: Spacing.two },
  rowTitle: { flex: 1 },
  dot: { width: 8, height: 8, borderRadius: 4 },
});
