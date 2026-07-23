import { BottomSheetModalProvider } from '@gorhom/bottom-sheet';
import { DarkTheme, DefaultTheme, Stack, ThemeProvider } from 'expo-router';
import { StatusBar } from 'expo-status-bar';
import { useEffect } from 'react';
import { StyleSheet } from 'react-native';
import { GestureHandlerRootView } from 'react-native-gesture-handler';
import { KeyboardProvider } from 'react-native-keyboard-controller';

import { useColorScheme } from '@/hooks/use-color-scheme';
import { useTheme } from '@/hooks/use-theme';
import { useThemePreference } from '@/stores/theme-preference';

export default function RootLayout() {
  const colorScheme = useColorScheme();
  const theme = useTheme();
  const load = useThemePreference((s) => s.load);

  // Read the stored override once at startup. Until it resolves the app renders
  // the OS scheme, which is the right default to flash: someone who never set a
  // preference sees no change at all, and someone who did sees at most one frame
  // of the system theme rather than a spinner.
  useEffect(() => {
    void load();
  }, [load]);

  return (
    // GestureHandlerRootView must be the outermost wrapper for gorhom's sheet
    // pan gestures to reach the touch system; BottomSheetModalProvider is the
    // portal host every `Sheet` presents into.
    <GestureHandlerRootView style={styles.root}>
      <KeyboardProvider>
        <ThemeProvider value={colorScheme === 'dark' ? DarkTheme : DefaultTheme}>
          <BottomSheetModalProvider>
          <StatusBar style={colorScheme === 'dark' ? 'light' : 'dark'} />
          <Stack screenOptions={{ contentStyle: { backgroundColor: theme.background } }}>
            <Stack.Screen name="index" options={{ title: 'OxiMux' }} />
            <Stack.Screen name="pair-scan" options={{ title: 'Scan pairing code' }} />
            <Stack.Screen name="sessions" options={{ title: 'Sessions' }} />
            <Stack.Screen name="settings" options={{ title: 'Settings' }} />
            <Stack.Screen name="schedules" options={{ title: 'Schedules' }} />
            {/* Titles come from the screens themselves: a session is named by the
                agent, the git screen by the branch it is on, and a schedule's run
                history by the schedule's own name. */}
            <Stack.Screen name="session/[id]" />
            <Stack.Screen name="git/[id]" />
            <Stack.Screen name="schedules/[id]" />
          </Stack>
          </BottomSheetModalProvider>
        </ThemeProvider>
      </KeyboardProvider>
    </GestureHandlerRootView>
  );
}

const styles = StyleSheet.create({ root: { flex: 1 } });
