import {
  ArrowDown,
  ArrowLeft,
  ArrowRight,
  ArrowUp,
  ChevronsDown,
  ChevronsUp,
  Minus,
  Plus,
} from 'lucide-react-native';
import { memo } from 'react';
import { Pressable, ScrollView, StyleSheet, View } from 'react-native';

import { ThemedText } from '@/components/themed-text';
import { Icon } from '@/components/ui/icon';
import { ControlHeight } from '@/components/ui/control-geometry';
import { Radius, Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { tick } from '@/native/haptics';

/**
 * The keys a phone's soft keyboard does not have.
 *
 * Without this a remote terminal is close to unusable: there is no Escape (so no
 * way out of an agent's prompt or a modal editor), no Tab (no completion), no
 * arrows (no history, no menu), and no Ctrl (no interrupting anything). Every
 * one of those is a key a shell session needs constantly and iOS/Android simply
 * do not offer — and `hideKeyboardAccessoryView` on the WebView removes the one
 * place the OS would have put them.
 *
 * Ctrl is a sticky modifier rather than a chord: there is no way to hold a key
 * on a touch screen. It is armed here and applied in the page, because the
 * keystroke it modifies is typed into xterm's own textarea and never reaches
 * React Native.
 */
export const KeyBar = memo(function KeyBar({
  onKey,
  onCtrlToggle,
  onZoom,
  onScroll,
  ctrlArmed,
}: {
  /** Send a raw byte sequence, exactly as a real terminal would emit it. */
  onKey: (sequence: string) => void;
  onCtrlToggle: () => void;
  onZoom: (delta: number) => void;
  /** Whole lines; negative scrolls back. */
  onScroll: (lines: number) => void;
  ctrlArmed: boolean;
}) {
  const theme = useTheme();

  return (
    <View style={[styles.bar, { backgroundColor: theme.surface1, borderTopColor: theme.border }]}>
      <ScrollView
        horizontal
        keyboardShouldPersistTaps="always"
        showsHorizontalScrollIndicator={false}
        contentContainerStyle={styles.row}
      >
        <Key label="esc" onPress={() => onKey('\x1b')} />
        <Key label="tab" onPress={() => onKey('\t')} />
        <Key label="ctrl" onPress={onCtrlToggle} active={ctrlArmed} />
        {/* The two interrupts worth a dedicated key: everything else is a Ctrl
            chord away, but these are what a stuck agent or a hung command needs
            and neither should cost two taps. */}
        <Key label="^C" onPress={() => onKey('\x03')} />
        <Key label="^D" onPress={() => onKey('\x04')} />

        <Divider />
        <Key icon={ArrowLeft} label="left" onPress={() => onKey('\x1b[D')} />
        <Key icon={ArrowUp} label="up" onPress={() => onKey('\x1b[A')} />
        <Key icon={ArrowDown} label="down" onPress={() => onKey('\x1b[B')} />
        <Key icon={ArrowRight} label="right" onPress={() => onKey('\x1b[C')} />

        <Divider />
        {/* Scrollback, not input: a touch drag over the grid is xterm's own
            selection gesture, so paging has to be a control. */}
        <Key icon={ChevronsUp} label="page up" onPress={() => onScroll(-10)} />
        <Key icon={ChevronsDown} label="page down" onPress={() => onScroll(10)} />

        <Divider />
        <Key icon={Minus} label="smaller text" onPress={() => onZoom(-1)} />
        <Key icon={Plus} label="larger text" onPress={() => onZoom(1)} />
      </ScrollView>
    </View>
  );
});

function Divider() {
  const theme = useTheme();
  return <View style={[styles.divider, { backgroundColor: theme.border }]} />;
}

function Key({
  label,
  icon,
  onPress,
  active,
}: {
  /** Also the accessibility label, so an icon-only key is still announced. */
  label: string;
  icon?: React.ComponentProps<typeof Icon>['icon'];
  onPress: () => void;
  active?: boolean;
}) {
  const theme = useTheme();
  return (
    <Pressable
      accessibilityRole="button"
      accessibilityLabel={label}
      accessibilityState={{ selected: !!active }}
      onPress={() => {
        tick();
        onPress();
      }}
      style={({ pressed }) => [
        styles.key,
        {
          backgroundColor: active ? theme.accent : theme.surface2,
          borderColor: active ? theme.accent : theme.border,
        },
        pressed && styles.pressed,
      ]}
    >
      {icon ? (
        <Icon icon={icon} size="md" color={active ? theme.accentText : theme.text} />
      ) : (
        <ThemedText type="smallBold" style={{ color: active ? theme.accentText : theme.text }}>
          {label}
        </ThemedText>
      )}
    </Pressable>
  );
}

const styles = StyleSheet.create({
  bar: { borderTopWidth: StyleSheet.hairlineWidth },
  row: { alignItems: 'center', gap: Spacing.two, paddingHorizontal: Spacing.two, paddingVertical: Spacing.two },
  key: {
    minWidth: ControlHeight.field,
    height: ControlHeight.compact,
    paddingHorizontal: Spacing.two,
    borderRadius: Radius.md,
    borderWidth: StyleSheet.hairlineWidth,
    alignItems: 'center',
    justifyContent: 'center',
  },
  pressed: { opacity: 0.6 },
  divider: { width: StyleSheet.hairlineWidth, alignSelf: 'stretch', marginVertical: Spacing.one },
});
