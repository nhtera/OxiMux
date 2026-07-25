import { Mic, MicOff } from 'lucide-react-native';
import { Pressable, StyleSheet } from 'react-native';

import { ControlHeight, IconHitSlop } from '@/components/ui/control-geometry';
import { Icon } from '@/components/ui/icon';
import { Radius } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';

type Props = {
  /** True once permission has been refused — the glyph says so. */
  denied: boolean;
  onStart: () => void;
};

/**
 * The composer's dictation trigger.
 *
 * Only ever the way *in*: once recording starts the composer hands the whole dock
 * to [`DictationBar`], which owns stopping, sending and discarding. So this stays
 * a single quiet glyph on the same circular step as the row's other controls,
 * with no meter, no timer and no second meaning.
 *
 * `denied` reads as a struck-through mic and still starts, because starting is
 * what surfaces the Settings hint — a button that did nothing would leave the
 * user with a greyed icon and no way to learn why.
 *
 * Rendered by the composer only while connected: the desktop is what transcribes,
 * so a phone with no host to ask has nothing to offer here.
 */
export function MicButton({ denied, onStart }: Props) {
  const theme = useTheme();

  return (
    <Pressable
      onPress={onStart}
      accessibilityRole="button"
      accessibilityLabel={denied ? 'Microphone access is off' : 'Dictate a prompt'}
      hitSlop={IconHitSlop}
      style={({ pressed }) => [styles.button, pressed && styles.pressed]}
    >
      <Icon
        icon={denied ? MicOff : Mic}
        size="lg"
        color={denied ? theme.textMuted : theme.textSecondary}
      />
    </Pressable>
  );
}

const styles = StyleSheet.create({
  button: {
    width: ControlHeight.compact,
    height: ControlHeight.compact,
    borderRadius: Radius.full,
    alignItems: 'center',
    justifyContent: 'center',
  },
  pressed: { opacity: 0.6 },
});
