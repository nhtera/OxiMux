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
