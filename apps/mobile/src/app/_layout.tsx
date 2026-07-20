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
      </Stack>
    </ThemeProvider>
  );
}
