import { Link, router } from 'expo-router';
import { useEffect, useState } from 'react';
import { ActivityIndicator, StyleSheet } from 'react-native';
import { SafeAreaView } from 'react-native-safe-area-context';

import { ThemedText } from '@/components/themed-text';
import { ThemedView } from '@/components/themed-view';
import { MaxContentWidth, Spacing } from '@/constants/theme';
import { useClient } from '@/native/client';
import { describeError } from '@/native/errors';

/**
 * The entry screen. A device that has already paired goes straight to its
 * sessions; everything else lands on the pairing prompt.
 */
export default function HomeScreen() {
  const resume = useClient((s) => s.resume);
  const [checking, setChecking] = useState(true);
  const [error, setError] = useState<string>();

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        if (await resume()) {
          if (!cancelled) router.replace('/sessions');
          return;
        }
      } catch (e) {
        // A stored host that will not answer must not trap the user on a spinner —
        // fall through to the pair prompt with the reason shown.
        if (!cancelled) setError(describeError(e));
      }
      if (!cancelled) setChecking(false);
    })();
    return () => {
      cancelled = true;
    };
  }, [resume]);

  if (checking && !error) {
    return (
      <ThemedView style={styles.container}>
        <SafeAreaView style={styles.safeArea}>
          <ActivityIndicator />
        </SafeAreaView>
      </ThemedView>
    );
  }

  return (
    <ThemedView style={styles.container}>
      <SafeAreaView style={styles.safeArea}>
        <ThemedText type="title">OxiMux</ThemedText>
        <ThemedText type="small" style={styles.centre}>
          {error
            ? `Could not reach the paired desktop: ${error}`
            : 'Pair with a desktop to drive its agent sessions from here.'}
        </ThemedText>
        <Link href="/pair-scan" style={styles.link}>
          <ThemedText type="code">Scan pairing code</ThemedText>
        </Link>
      </SafeAreaView>
    </ThemedView>
  );
}

const styles = StyleSheet.create({
  container: { flex: 1, justifyContent: 'center', flexDirection: 'row' },
  safeArea: {
    flex: 1,
    justifyContent: 'center',
    alignItems: 'center',
    gap: Spacing.three,
    paddingHorizontal: Spacing.four,
    maxWidth: MaxContentWidth,
  },
  centre: { textAlign: 'center' },
  link: { paddingVertical: Spacing.three },
});
