import { DarkTheme, DefaultTheme, Stack, ThemeProvider } from 'expo-router';
import { StatusBar } from 'expo-status-bar';
import { useEffect } from 'react';
import { StyleSheet } from 'react-native';
import { GestureHandlerRootView } from 'react-native-gesture-handler';
import { KeyboardProvider } from 'react-native-keyboard-controller';
import { SafeAreaProvider } from 'react-native-safe-area-context';

import { AppDrawerProvider, DrawerMenuButton } from '@/components/deck/app-drawer';
import { useColorScheme } from '@/hooks/use-color-scheme';
import { useTheme } from '@/hooks/use-theme';
import { useChatPreferences } from '@/stores/chat-preferences';
import { useThemePreference } from '@/stores/theme-preference';

export default function RootLayout() {
  const colorScheme = useColorScheme();
  const theme = useTheme();
  const load = useThemePreference((s) => s.load);
  const loadChatPrefs = useChatPreferences((s) => s.load);

  // Read the stored override once at startup. Until it resolves the app renders
  // the OS scheme, which is the right default to flash: someone who never set a
  // preference sees no change at all, and someone who did sees at most one frame
  // of the system theme rather than a spinner. Chat preferences load alongside;
  // their default (overview, thinking collapsed) is also the safe pre-load render.
  useEffect(() => {
    void load();
    void loadChatPrefs();
  }, [load, loadChatPrefs]);

  const menuButton = () => <DrawerMenuButton />;

  return (
    // GestureHandlerRootView must be the outermost wrapper so the sheet + drawer
    // pan gestures reach the touch system; the in-house `Sheet` renders inline
    // under it (no portal provider) and rises with the keyboard via KeyboardProvider.
    // SafeAreaProvider sits above the drawer overlay (a sibling of the navigator)
    // so its panel reads real insets rather than the zero fallback.
    <GestureHandlerRootView style={styles.root}>
      <SafeAreaProvider>
        <KeyboardProvider>
          <ThemeProvider value={colorScheme === 'dark' ? DarkTheme : DefaultTheme}>
            <StatusBar style={colorScheme === 'dark' ? 'light' : 'dark'} />
            {/* The nav drawer wraps the whole navigator: a single overlay reachable
                from every primary screen's header, matching the desktop's sidebar. */}
            <AppDrawerProvider>
              <Stack screenOptions={{ contentStyle: { backgroundColor: theme.background } }}>
                <Stack.Screen name="index" options={{ title: 'OxiMux' }} />
                <Stack.Screen name="pair-scan" options={{ title: 'Scan pairing code' }} />
                {/* The four primary destinations carry the hamburger and switch with
                    no slide — they are drawer siblings, not a push stack, so the new
                    screen just appears while the drawer closes (the reference does the
                    same). Detail routes below keep their back button and push slide. */}
                <Stack.Screen name="sessions" options={{ title: 'Sessions', headerLeft: menuButton, animation: 'none' }} />
                <Stack.Screen name="terminals" options={{ title: 'Terminals', headerLeft: menuButton, animation: 'none' }} />
                <Stack.Screen name="settings" options={{ title: 'Settings', headerLeft: menuButton, animation: 'none' }} />
                <Stack.Screen name="schedules" options={{ title: 'Schedules', headerLeft: menuButton, animation: 'none' }} />
                {/* Titles come from the screens themselves: a session is named by the
                    agent, the git screen by the branch it is on, and a schedule's run
                    history by the schedule's own name. */}
                <Stack.Screen name="session/[id]" />
                <Stack.Screen name="git/[id]" />
                <Stack.Screen name="schedules/[id]" />
              </Stack>
            </AppDrawerProvider>
          </ThemeProvider>
        </KeyboardProvider>
      </SafeAreaProvider>
    </GestureHandlerRootView>
  );
}

const styles = StyleSheet.create({ root: { flex: 1 } });
