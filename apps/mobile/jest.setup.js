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
