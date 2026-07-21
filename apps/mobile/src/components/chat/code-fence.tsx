import CodeHighlighter from 'react-native-code-highlighter';
import { atomOneDark, atomOneLight } from 'react-syntax-highlighter/dist/esm/styles/hljs';
import { StyleSheet, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Fonts, Spacing } from '@/constants/theme';
import { useColorScheme } from '@/hooks/use-color-scheme';
import { useTheme } from '@/hooks/use-theme';

/**
 * A fenced code block from an agent reply, syntax-highlighted.
 *
 * Highlighting runs in pure JS (highlight.js via `react-syntax-highlighter`) and
 * paints native `Text` runs — no WebView and no WASM. A WebView per fence would
 * be a browser instance per code block inside a virtualized transcript, which is
 * the cost this whole screen is built to avoid.
 *
 * The body scrolls **horizontally rather than wrapping**, matching the git diff
 * view: wrapping a code line destroys the indentation that carries its structure,
 * and indentation is most of what makes code skimmable on a small screen.
 *
 * An unknown or absent language tag is not an error — highlight.js is left to
 * auto-detect, and if it cannot, the text still renders as plain monospace. A
 * fence must always show its content; failing to colour it is cosmetic, failing
 * to display it is not.
 */
export function CodeFence({ code, language }: { code: string; language?: string }) {
  const theme = useTheme();
  const scheme = useColorScheme();
  const hljsStyle = scheme === 'dark' ? atomOneDark : atomOneLight;

  // highlight.js treats an unregistered language as fatal in strict mode; passing
  // undefined lets it auto-detect instead, which degrades to plain text.
  const resolved = language && language.trim().length > 0 ? language : undefined;

  return (
    <View style={[styles.wrap, { backgroundColor: theme.backgroundElement }]}>
      {resolved ? (
        <ThemedText type="small" style={styles.lang}>
          {resolved}
        </ThemedText>
      ) : null}
      <CodeHighlighter
        hljsStyle={hljsStyle}
        language={resolved}
        textStyle={styles.code}
        scrollViewProps={{
          horizontal: true,
          showsHorizontalScrollIndicator: false,
          contentContainerStyle: styles.codeContent,
        }}
      >
        {code}
      </CodeHighlighter>
    </View>
  );
}

const styles = StyleSheet.create({
  wrap: {
    borderRadius: Spacing.two,
    paddingVertical: Spacing.two,
    gap: Spacing.half,
  },
  lang: { opacity: 0.5, paddingHorizontal: Spacing.two },
  code: { fontFamily: Fonts?.mono, fontSize: 13, lineHeight: 18 },
  codeContent: { paddingHorizontal: Spacing.two },
});
