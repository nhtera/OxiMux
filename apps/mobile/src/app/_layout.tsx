import { DarkTheme, DefaultTheme, Stack, ThemeProvider } from 'expo-router';
import { useEffect } from 'react';

import { useColorScheme } from '@/hooks/use-color-scheme';
import { useThemePreference } from '@/stores/theme-preference';

export default function RootLayout() {
  const colorScheme = useColorScheme();
  const load = useThemePreference((s) => s.load);

  // Read the stored override once at startup. Until it resolves the app renders
  // the OS scheme, which is the right default to flash: someone who never set a
  // preference sees no change at all, and someone who did sees at most one frame
  // of the system theme rather than a spinner.
  useEffect(() => {
    void load();
  }, [load]);

  return (
    <ThemeProvider value={colorScheme === 'dark' ? DarkTheme : DefaultTheme}>
      <Stack>
        <Stack.Screen name="index" options={{ title: 'OxiMux' }} />
        <Stack.Screen name="pair-scan" options={{ title: 'Scan pairing code' }} />
        <Stack.Screen name="sessions" options={{ title: 'Sessions' }} />
        <Stack.Screen name="settings" options={{ title: 'Settings' }} />
        {/* Titles come from the screens themselves: a session is named by the
            agent, and the git screen by the branch it is on. */}
        <Stack.Screen name="session/[id]" />
        <Stack.Screen name="git/[id]" />
      </Stack>
    </ThemeProvider>
  );
}
