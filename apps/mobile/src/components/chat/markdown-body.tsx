import { useMemo } from 'react';
import { Linking, StyleSheet, Text, View, type TextStyle, type ViewStyle } from 'react-native';
import { Renderer, useMarkdown, type RendererInterface } from 'react-native-marked';

import { CodeFence } from '@/components/chat/code-fence';
import { Fonts, Spacing } from '@/constants/theme';
import { useColorScheme } from '@/hooks/use-color-scheme';
import { useTheme } from '@/hooks/use-theme';

/**
 * Schemes a markdown link is allowed to open.
 *
 * Agent output is *model-generated text*, and a model can be steered by whatever
 * it read — a file, a web page, a tool result. So a link in a reply is untrusted
 * input, not something the user chose to tap into. `javascript:` and `data:`
 * URIs are the classic way that becomes code execution or a spoofed document, so
 * the allowlist is positive: anything not named here does not open.
 */
const SAFE_SCHEMES = ['http:', 'https:', 'mailto:'];

function openIfSafe(href: string) {
  let parsed: { protocol: string };
  try {
    parsed = new URL(href);
  } catch {
    // A relative or malformed href has no origin to open against. Silently
    // ignoring is right here: it is far more likely to be a model writing a bare
    // path than anything the user wants a browser for.
    return;
  }
  if (!SAFE_SCHEMES.includes(parsed.protocol)) return;
  void Linking.openURL(href).catch(() => {
    // No handler for the scheme, or the OS refused. Nothing useful to say — the
    // user tapped a link and nothing happened, which is the correct outcome for
    // a link that cannot be opened.
  });
}

/**
 * Renders fenced code through [`CodeFence`] and routes links through the scheme
 * allowlist above, leaving every other token to the library's defaults.
 */
class ChatRenderer extends Renderer implements RendererInterface {
  code(text: string, language?: string): React.ReactNode {
    return <CodeFence key={this.getKey()} code={text} language={language} />;
  }

  link(children: string | React.ReactNode[], href: string, styles?: TextStyle): React.ReactNode {
    return (
      <Text key={this.getKey()} style={styles} onPress={() => openIfSafe(href)}>
        {children}
      </Text>
    );
  }
}

/**
 * An agent's message body, rendered as markdown.
 *
 * Uses the `useMarkdown` **hook** rather than the library's `<Markdown>`
 * component on purpose: that component renders its blocks through an internal
 * `FlatList`, and this sits inside the transcript's own `FlatList`. Nesting
 * virtualized lists on the same axis breaks windowing and warns at runtime. The
 * hook hands back plain nodes, so the transcript stays the only list.
 *
 * The renderer instance and the style objects are memoized because the hook
 * re-parses whenever they change identity — and this component re-renders on
 * every streamed token. Rebuilding them inline would re-parse the entire message
 * on each delta of a long reply.
 */
export function MarkdownBody({ text }: { text: string }) {
  const theme = useTheme();
  const scheme = useColorScheme();

  const renderer = useMemo(() => new ChatRenderer(), []);

  const styles = useMemo(
    () => markdownStyles(theme.text, theme.textSecondary, theme.backgroundElement),
    [theme.text, theme.textSecondary, theme.backgroundElement]
  );

  const nodes = useMarkdown(text, {
    renderer,
    styles,
    colorScheme: scheme === 'unspecified' ? 'light' : scheme,
  });

  return <View style={base.body}>{nodes}</View>;
}

function markdownStyles(
  text: string,
  secondary: string,
  elementBackground: string
): Record<string, TextStyle | ViewStyle> {
  return {
    text: { color: text, fontSize: 15, lineHeight: 22 },
    paragraph: { paddingVertical: Spacing.half },
    // The library's link default is a system blue that reads as untappable grey
    // on the dark theme; the app's own text colour with an underline carries the
    // affordance without depending on hue.
    link: { color: text, textDecorationLine: 'underline' },
    strong: { fontWeight: '600' },
    em: { fontStyle: 'italic' },
    h1: { color: text, fontSize: 22, fontWeight: '700', marginTop: Spacing.two },
    h2: { color: text, fontSize: 19, fontWeight: '700', marginTop: Spacing.two },
    h3: { color: text, fontSize: 17, fontWeight: '600', marginTop: Spacing.one },
    h4: { color: text, fontSize: 15, fontWeight: '600' },
    h5: { color: text, fontSize: 14, fontWeight: '600' },
    h6: { color: secondary, fontSize: 13, fontWeight: '600' },
    codespan: {
      fontFamily: Fonts?.mono,
      fontSize: 13,
      color: text,
      backgroundColor: elementBackground,
    },
    blockquote: {
      borderLeftWidth: 3,
      borderLeftColor: secondary,
      paddingLeft: Spacing.two,
      opacity: 0.85,
    },
    li: { color: text, fontSize: 15, lineHeight: 22 },
    hr: { borderBottomColor: secondary, opacity: 0.3 },
    table: { borderColor: secondary },
    tableCell: { borderColor: secondary },
  };
}

const base = StyleSheet.create({
  body: { gap: 0 },
});
