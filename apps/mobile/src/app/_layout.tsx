import { DarkTheme, DefaultTheme, Stack, ThemeProvider } from 'expo-router';
import { useColorScheme } from 'react-native';

export default function RootLayout() {
  const colorScheme = useColorScheme();
  return (
    <ThemeProvider value={colorScheme === 'dark' ? DarkTheme : DefaultTheme}>
      <Stack>
        <Stack.Screen name="index" options={{ title: 'OxiMux' }} />
        <Stack.Screen name="pair-scan" options={{ title: 'Scan pairing code' }} />
        <Stack.Screen name="sessions" options={{ title: 'Sessions' }} />
        {/* Titles come from the screens themselves: a session is named by the
            agent, and the git screen by the branch it is on. */}
        <Stack.Screen name="session/[id]" />
        <Stack.Screen name="git/[id]" />
      </Stack>
    </ThemeProvider>
  );
}
