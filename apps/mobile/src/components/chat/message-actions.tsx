import { Check, Copy } from 'lucide-react-native';
import { useCallback, useRef, useState } from 'react';
import { Pressable, StyleSheet, View } from 'react-native';

import { Icon } from '@/components/ui/icon';
import { ThemedText } from '@/components/themed-text';
import { Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { copyText } from '@/native/clipboard';

/** How long the button holds its "Copied" state before reverting. */
const COPIED_MS = 1500;

/**
 * The trailing action row under a message bubble. On a phone the actions are
 * always visible (there is no hover to reveal them), kept quiet with muted
 * colour and small type so they read as secondary to the message itself.
 *
 * Copy is the only action for now: fork-a-turn needs projection fields the wire
 * does not carry yet, so it waits for the proto phase that adds them rather than
 * being faked here.
 */
export function MessageActions({ text }: { text: string }) {
  const theme = useTheme();
  const [copied, setCopied] = useState(false);
  // Cleared on unmount so a copy fired just before the bubble scrolls out of the
  // window does not set state on a gone component.
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined);

  const onCopy = useCallback(async () => {
    if (!text) return;
    const ok = await copyText(text);
    if (!ok) return;
    setCopied(true);
    clearTimeout(timer.current);
    timer.current = setTimeout(() => setCopied(false), COPIED_MS);
  }, [text]);

  if (!text) return null;

  return (
    <View style={styles.row}>
      <Pressable
        onPress={onCopy}
        accessibilityRole="button"
        accessibilityLabel={copied ? 'Copied' : 'Copy message'}
        hitSlop={Spacing.two}
        style={({ pressed }) => [styles.action, pressed && styles.pressed]}
      >
        <Icon
          icon={copied ? Check : Copy}
          size="sm"
          color={copied ? theme.success : theme.textSecondary}
        />
        <ThemedText type="small" themeColor={copied ? 'success' : 'textSecondary'}>
          {copied ? 'Copied' : 'Copy'}
        </ThemedText>
      </Pressable>
    </View>
  );
}

const styles = StyleSheet.create({
  row: { flexDirection: 'row', alignItems: 'center', gap: Spacing.three },
  action: { flexDirection: 'row', alignItems: 'center', gap: Spacing.one },
  pressed: { opacity: 0.6 },
});
