/**
 * Global test setup.
 *
 * AsyncStorage is mocked for every suite, not just the ones that store anything.
 * The theme preference is read through `useColorScheme`, which `useTheme` and
 * therefore `ThemedText` depend on — so any component test at all now pulls
 * storage in transitively, and without this it fails on a missing native module
 * far from anything the test is about.
 *
 * The mock ships with the package, so it tracks the real API rather than a
 * hand-written stand-in that would drift.
 */
jest.mock('@react-native-async-storage/async-storage', () =>
  require('@react-native-async-storage/async-storage/jest/async-storage-mock')
);

/**
 * The dictation hook binds two native surfaces at import — `expo-audio`'s
 * recorder and, through the client, the `OximuxCore` TurboModule — neither of
 * which exists in the Node test environment. Any component that pulls it in (the
 * composer, transitively) would die on a missing native module far from what the
 * test is about. The real record → transcribe path runs against the desktop
 * engine and is covered in Rust; here the hook is a stub reporting "not
 * available", so the composer renders without the mic button, which is exactly
 * its disconnected state.
 */
/**
 * Safe-area insets need a `<SafeAreaProvider>` at the tree root, which unit tests
 * do not mount. The `Sheet` primitive reads insets, so any test rendering a sheet
 * would throw. The package's own mock returns zero insets — the right value for a
 * headless render — and tracks the real API.
 */
jest.mock('react-native-safe-area-context', () => {
  const React = require('react');
  const { View } = require('react-native');
  const inset = { top: 0, bottom: 0, left: 0, right: 0 };
  const frame = { x: 0, y: 0, width: 0, height: 0 };
  const Passthrough = ({ children }) => React.createElement(View, null, children);
  return {
    SafeAreaProvider: Passthrough,
    SafeAreaView: View,
    SafeAreaInsetsContext: React.createContext(inset),
    useSafeAreaInsets: () => inset,
    useSafeAreaFrame: () => frame,
    initialWindowMetrics: { insets: inset, frame },
  };
});

/**
 * The in-house `Sheet` (and the swipe-deck drawer) drive gesture-handler +
 * reanimated + keyboard-controller directly. Under Node those reach for native
 * pieces that do not exist, so any sheet a test renders (the choice picker, the
 * rewind sheet) would die on a missing native module.
 *
 * keyboard-controller ships a jest stand-in that tracks the real API — its `/jest`
 * entry returns a zero-height keyboard, the right value for a headless render.
 * reanimated v4's shipped mock, though, pulls its real native initializers and
 * cannot load under Node, and its `GestureDetector` in turn drives reanimated
 * internals — so both get small hand-written stubs covering exactly the surface
 * these components use. Animated hosts pass children through, shared values are
 * plain boxes, the timing/spring helpers resolve instantly (firing their completion
 * callback), and gestures are inert chainable builders — so a `visible` sheet
 * renders its content exactly as on device, which is what every sheet test asserts.
 */
jest.mock('react-native-keyboard-controller', () =>
  require('react-native-keyboard-controller/jest')
);
jest.mock('react-native-gesture-handler', () => {
  const { View } = require('react-native');
  // Every builder method returns the builder, so any `.onEnd().onUpdate()` chain
  // resolves to an inert object; a detector just renders its child.
  const builder = () => {
    const g = new Proxy(() => g, { get: () => (() => g) });
    return g;
  };
  return {
    __esModule: true,
    GestureHandlerRootView: View,
    GestureDetector: ({ children }) => children ?? null,
    GestureBuilder: builder,
    Gesture: new Proxy({}, { get: () => builder }),
    Directions: {},
    State: {},
  };
});
jest.mock('react-native-reanimated', () => {
  const { View, ScrollView, Text, Image } = require('react-native');
  const box = (init) => ({ value: init });
  const Animated = { View, ScrollView, Text, Image, createAnimatedComponent: (c) => c };
  return {
    __esModule: true,
    default: Animated,
    useSharedValue: box,
    useAnimatedStyle: (fn) => {
      try {
        return fn();
      } catch {
        return {};
      }
    },
    withSpring: (v) => v,
    withTiming: (v, _cfg, cb) => {
      if (cb) cb(true);
      return v;
    },
    withRepeat: (v) => v,
    runOnJS:
      (fn) =>
      (...args) =>
        fn(...args),
    interpolate: (_x, _inRange, outRange) => outRange[0],
    interpolateColor: (_x, _inRange, outRange) => outRange[0],
    Extrapolation: { CLAMP: 'clamp', EXTEND: 'extend', IDENTITY: 'identity' },
    // `bezier`/`out`/… are *called* at module load (constants/motion.ts), so they
    // must be real functions, not the bare `{}` a passed-but-never-invoked easing
    // could get away with. They return an identity curve — jest never animates.
    Easing: {
      bezier: () => (t) => t,
      linear: (t) => t,
      ease: (t) => t,
      in: (fn) => fn,
      out: (fn) => fn,
      inOut: (fn) => fn,
      cubic: (t) => t,
    },
  };
});

jest.mock('@/native/use-dictation', () => ({
  useDictation: () => ({
    phase: 'idle',
    level: 0,
    available: false,
    start: jest.fn(),
    stop: jest.fn(),
    cancel: jest.fn(),
  }),
}));
