import { useColorScheme as useOsColorScheme } from 'react-native';

import { useThemePreference } from '@/stores/theme-preference';

/**
 * The colour scheme the app should render in: the user's explicit choice when
 * they made one, otherwise the OS setting.
 *
 * Wraps React Native's hook rather than replacing it, so every existing caller
 * picks the override up unchanged — `useTheme` and the components reading it did
 * not have to learn that a preference exists.
 */
export function useColorScheme() {
  const os = useOsColorScheme();
  const preference = useThemePreference((s) => s.preference);
  if (preference === 'system') return os;
  return preference;
}
