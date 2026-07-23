import {
  BottomSheetBackdrop,
  type BottomSheetBackdropProps,
  BottomSheetModal,
  BottomSheetScrollView,
} from '@gorhom/bottom-sheet';
import { useCallback, useEffect, useRef } from 'react';
import { StyleSheet, type StyleProp, type ViewStyle } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';

import { Radius, Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';

/** Re-exported so sheets swap their text fields to the sheet-aware input, which
 *  is required — a plain `TextInput` fights the sheet's own pan gesture. */
export { BottomSheetTextInput } from '@gorhom/bottom-sheet';

type Props = {
  visible: boolean;
  onClose: () => void;
  children: React.ReactNode;
  contentStyle?: StyleProp<ViewStyle>;
};

/**
 * The one bottom sheet. A real gesture-driven sheet (drag-to-dismiss, spring,
 * keyboard-aware, backdrop) behind a plain `visible`/`onClose` prop, so callers
 * keep the controlled API the old hand-rolled `Modal` sheets used. Sizes to its
 * content (`enableDynamicSizing`, gorhom's default) and scrolls internally once
 * the content would exceed the screen — no per-sheet snap-point guessing.
 */
export function Sheet({ visible, onClose, children, contentStyle }: Props) {
  const theme = useTheme();
  const insets = useSafeAreaInsets();
  const ref = useRef<BottomSheetModal>(null);

  // Drive present/dismiss from the controlled `visible` prop rather than making
  // every caller learn the imperative ref API.
  useEffect(() => {
    if (visible) ref.current?.present();
    else ref.current?.dismiss();
  }, [visible]);

  const renderBackdrop = useCallback(
    (props: BottomSheetBackdropProps) => (
      <BottomSheetBackdrop {...props} appearsOnIndex={0} disappearsOnIndex={-1} pressBehavior="close" />
    ),
    []
  );

  return (
    <BottomSheetModal
      ref={ref}
      enablePanDownToClose
      onDismiss={onClose}
      topInset={insets.top + Spacing.three}
      keyboardBehavior="extend"
      keyboardBlurBehavior="restore"
      backdropComponent={renderBackdrop}
      backgroundStyle={{ backgroundColor: theme.surface1, borderTopLeftRadius: Radius.xl, borderTopRightRadius: Radius.xl }}
      handleIndicatorStyle={{ backgroundColor: theme.borderStrong }}
    >
      <BottomSheetScrollView
        contentContainerStyle={[styles.content, { paddingBottom: insets.bottom + Spacing.four }, contentStyle]}
      >
        {children}
      </BottomSheetScrollView>
    </BottomSheetModal>
  );
}

const styles = StyleSheet.create({
  content: {
    paddingHorizontal: Spacing.four,
    paddingTop: Spacing.two,
    gap: Spacing.two,
  },
});
