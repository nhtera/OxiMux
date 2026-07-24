import { router, usePathname } from 'expo-router';
import {
  CalendarClock,
  Circle,
  Menu,
  MessagesSquare,
  Plus,
  Settings,
  SquareTerminal,
  type LucideIcon,
} from 'lucide-react-native';
import type { SessionSummary } from 'oximux-core';
import { createContext, useCallback, useContext, useMemo, useState } from 'react';
import { Dimensions, Pressable, ScrollView, StyleSheet, View } from 'react-native';
import { Gesture } from 'react-native-gesture-handler';
import Animated, {
  Extrapolation,
  interpolate,
  runOnJS,
  useAnimatedStyle,
  useSharedValue,
  withTiming,
  type SharedValue,
} from 'react-native-reanimated';
import { useSafeAreaInsets } from 'react-native-safe-area-context';

import { ThemedText } from '@/components/themed-text';
import { Icon } from '@/components/ui/icon';
import { IconButton } from '@/components/ui/icon-button';
import { ListDivider } from '@/components/ui/list-row';
import { MOTION_TIMING } from '@/constants/motion';
import { Radius, Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { useClient } from '@/native/client';
import { tick } from '@/native/haptics';
import { useNewSessionIntent } from '@/stores/new-session-intent';

const SCREEN = Dimensions.get('window').width;
/** Panel width — most of the screen, but never full-bleed so the screen peeks. */
export const DRAWER_WIDTH = Math.min(320, SCREEN * 0.84);
/** Flick past a third of the panel, or fast, to commit open/closed. */
const COMMIT_FRACTION = DRAWER_WIDTH / 3;
const COMMIT_VELOCITY = 500;
/** Left-edge grab zone for opening the drawer from a screen. */
const EDGE_ZONE = 28;

/** The four flat, always-relevant destinations. Detail routes are pushed on top
 *  of whichever of these is active and are not listed here. */
const DESTINATIONS: { label: string; icon: LucideIcon; path: '/sessions' | '/terminals' | '/schedules' | '/settings' }[] = [
  { label: 'Sessions', icon: MessagesSquare, path: '/sessions' },
  { label: 'Terminals', icon: SquareTerminal, path: '/terminals' },
  { label: 'Schedules', icon: CalendarClock, path: '/schedules' },
  { label: 'Settings', icon: Settings, path: '/settings' },
];

type DrawerApi = {
  open: () => void;
  close: () => void;
  isOpen: boolean;
  progress: SharedValue<number>;
  /** Left-edge pan a screen can attach to open/close the drawer by dragging. */
  edgePan: ReturnType<typeof Gesture.Pan>;
};

const DrawerContext = createContext<DrawerApi | null>(null);

/** Header menu button, session-switch button, or any screen reaches the one drawer
 *  through this. Throws if used outside the provider so a missing wrap is loud. */
export function useAppDrawer(): DrawerApi {
  const api = useContext(DrawerContext);
  if (!api) throw new Error('useAppDrawer must be used within AppDrawerProvider');
  return api;
}

/** The hamburger for a primary screen's `headerLeft` — opens the nav drawer. */
export function DrawerMenuButton() {
  const { open } = useAppDrawer();
  return <IconButton icon={Menu} accessibilityLabel="Open navigation menu" onPress={open} />;
}

/**
 * The one app drawer: primary navigation as a left panel over whatever screen is
 * showing. A single `progress` value (0 closed → 1 open) drives the panel slide
 * and the backdrop dim together so a drag never desyncs them; a React `isOpen`
 * mirror gates backdrop touches. Modeled on the verified swipe-deck, hoisted here
 * so every screen shares one drawer instead of each rebuilding its own.
 */
export function AppDrawerProvider({ children }: { children: React.ReactNode }) {
  const progress = useSharedValue(0);
  const [isOpen, setIsOpen] = useState(false);

  const settle = useCallback(
    (toOpen: boolean) => {
      setIsOpen(toOpen);
      // Mutating a reanimated shared value is the intended API; the compiler's
      // immutability rule does not model shared values.
      // eslint-disable-next-line react-hooks/immutability
      progress.value = withTiming(toOpen ? 1 : 0, MOTION_TIMING);
    },
    [progress]
  );
  const open = useCallback(() => settle(true), [settle]);
  const close = useCallback(() => settle(false), [settle]);

  // Left-edge pan opens; a pan on the open drawer closes. Vertical intent fails
  // the gesture so it never fights a screen's own scroll.
  const edgePan = useMemo(
    () =>
      Gesture.Pan()
        .activeOffsetX([-12, 12])
        .failOffsetY([-16, 16])
        .onUpdate((e) => {
          'worklet';
          const fromOpen = progress.value > 0.5;
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
        }),
    [progress, settle]
  );

  const api = useMemo<DrawerApi>(() => ({ open, close, isOpen, progress, edgePan }), [open, close, isOpen, progress, edgePan]);

  return (
    <DrawerContext.Provider value={api}>
      {children}
      <DrawerPanel progress={progress} isOpen={isOpen} close={close} />
    </DrawerContext.Provider>
  );
}

function DrawerPanel({
  progress,
  isOpen,
  close,
}: {
  progress: SharedValue<number>;
  isOpen: boolean;
  close: () => void;
}) {
  const theme = useTheme();
  const insets = useSafeAreaInsets();
  const sessions = useClient((s) => s.sessions);
  const pathname = usePathname();

  const goto = useCallback(
    (path: string) => {
      close();
      if (pathname !== path) router.navigate(path as never);
    },
    [close, pathname]
  );
  const openSession = useCallback(
    (id: string) => {
      close();
      router.navigate({ pathname: '/session/[id]', params: { id } });
    },
    [close]
  );

  const backdropStyle = useAnimatedStyle(() => ({
    opacity: interpolate(progress.value, [0, 1], [0, 0.5], Extrapolation.CLAMP),
  }));
  const panelStyle = useAnimatedStyle(() => ({
    // Clamped so a drag or timing tail can never carry the panel past its open
    // position (the overshoot that read as a jump).
    transform: [{ translateX: interpolate(progress.value, [0, 1], [-DRAWER_WIDTH, 0], Extrapolation.CLAMP) }],
  }));

  return (
    <>
      <Animated.View pointerEvents={isOpen ? 'auto' : 'none'} onTouchEnd={close} style={[styles.backdrop, backdropStyle]} />
      <Animated.View
        pointerEvents={isOpen ? 'auto' : 'none'}
        style={[styles.panel, { backgroundColor: theme.surface1, borderRightColor: theme.border, paddingTop: insets.top + Spacing.two }, panelStyle]}
      >
        <ScrollView contentContainerStyle={styles.inner} showsVerticalScrollIndicator={false}>
          {DESTINATIONS.map((d) => {
            const active = pathname === d.path;
            return (
              <Pressable
                key={d.path}
                onPress={() => {
                  tick();
                  goto(d.path);
                }}
                style={({ pressed }) => [
                  styles.dest,
                  active && { backgroundColor: theme.surface2 },
                  pressed && { backgroundColor: theme.surface2 },
                ]}
              >
                <Icon icon={d.icon} size="md" color={active ? theme.accent : theme.textSecondary} />
                <ThemedText themeColor={active ? 'accent' : 'text'}>{d.label}</ThemedText>
              </Pressable>
            );
          })}

          <ListDivider inset={Spacing.three} />

          <View style={styles.sessionsHead}>
            <ThemedText type="small" themeColor="textMuted">
              Sessions
            </ThemedText>
            <Pressable
              onPress={() => {
                tick();
                // Actually start a new session — the "+" is a create affordance,
                // not a shortcut to the list. Raise the intent, then route to the
                // Sessions screen that owns the create sheet.
                useNewSessionIntent.getState().request();
                goto('/sessions');
              }}
              accessibilityLabel="New session"
              hitSlop={Spacing.two}
            >
              <Icon icon={Plus} size="sm" color={theme.textSecondary} />
            </Pressable>
          </View>

          {sessions.length === 0 ? (
            <ThemedText type="small" themeColor="textMuted" style={styles.emptyNote}>
              No sessions open on the desktop.
            </ThemedText>
          ) : (
            sessions.map((s: SessionSummary) => {
              const active = pathname === `/session/${s.sessionId}`;
              return (
                <Pressable
                  key={s.sessionId}
                  onPress={() => openSession(s.sessionId)}
                  style={({ pressed }) => [styles.sessionRow, (active || pressed) && { backgroundColor: theme.surface2 }]}
                >
                  <View style={styles.rowHead}>
                    {active ? <Icon icon={Circle} size="xs" color={theme.accent} /> : null}
                    <ThemedText numberOfLines={1} style={styles.rowTitle}>
                      {s.title}
                    </ThemedText>
                    {s.awaitingPermission ? <View style={[styles.dot, { backgroundColor: theme.warning }]} /> : null}
                  </View>
                  {s.model ? (
                    <ThemedText type="small" numberOfLines={1} themeColor="textSecondary">
                      {s.model}
                    </ThemedText>
                  ) : null}
                </Pressable>
              );
            })
          )}
        </ScrollView>
      </Animated.View>
    </>
  );
}

const styles = StyleSheet.create({
  // A plain black scrim reads the same in light and dark, so it stays fixed.
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
  inner: { paddingHorizontal: Spacing.two, paddingBottom: Spacing.four, gap: Spacing.half },
  dest: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: Spacing.three,
    paddingVertical: Spacing.three,
    paddingHorizontal: Spacing.two,
    borderRadius: Radius.md,
  },
  sessionsHead: {
    flexDirection: 'row',
    alignItems: 'center',
    justifyContent: 'space-between',
    paddingHorizontal: Spacing.two,
    paddingTop: Spacing.two,
    paddingBottom: Spacing.one,
  },
  emptyNote: { paddingHorizontal: Spacing.two, paddingVertical: Spacing.two },
  sessionRow: { paddingVertical: Spacing.two, paddingHorizontal: Spacing.two, borderRadius: Radius.md, gap: Spacing.half },
  rowHead: { flexDirection: 'row', alignItems: 'center', gap: Spacing.two },
  rowTitle: { flex: 1 },
  dot: { width: 8, height: 8, borderRadius: 4 },
});
