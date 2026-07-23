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
 * The bottom-sheet library leans on reanimated + gesture-handler native pieces
 * that do not exist under Node. Any sheet a test renders (the choice picker, the
 * rewind sheet) would otherwise die on a missing native module. The mock renders
 * children directly and stubs the imperative present/dismiss, so a test that
 * mounts a sheet `visible` sees its content exactly as on device — which is what
 * every sheet test asserts against.
 */
jest.mock('@gorhom/bottom-sheet', () => {
  const React = require('react');
  const { View, ScrollView, TextInput } = require('react-native');
  const Modal = React.forwardRef((props, ref) => {
    React.useImperativeHandle(ref, () => ({ present: () => {}, dismiss: () => {} }));
    return React.createElement(React.Fragment, null, props.children);
  });
  return {
    __esModule: true,
    default: View,
    BottomSheetModal: Modal,
    BottomSheetModalProvider: ({ children }) => children,
    BottomSheetScrollView: ScrollView,
    BottomSheetView: View,
    BottomSheetTextInput: TextInput,
    BottomSheetBackdrop: () => null,
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
