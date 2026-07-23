/**
 * Design tokens for the mobile app.
 *
 * One source of truth for colour, type, spacing, radius and elevation. Screens
 * pull semantic tokens (`danger`, `success`, `surface2`, …) from `useTheme()`
 * rather than hardcoding hex, so a single edit here recolours the whole app and
 * light/dark stay in lockstep. A lightweight typed-theme hook — no styling
 * engine — kept deliberately small so a later migration stays mechanical.
 */

import { Platform } from 'react-native';

/**
 * Per-scheme semantic colours.
 *
 * Surfaces are an elevation ramp (`surface0` app background → `surface3` highest
 * elevation), so "make this look more raised" is `surface1 → surface2`, a number
 * step, not a new colour name. Status colours are tuned per scheme: a red that
 * reads on white is muddy on black and vice-versa, so light uses darker, calmer
 * values and dark uses the brighter palette.
 */
type SchemeColors = {
  // Surfaces (elevation ramp)
  surface0: string;
  surface1: string;
  surface2: string;
  surface3: string;

  // Text
  text: string;
  textSecondary: string;
  textMuted: string;

  // Borders
  border: string;
  borderStrong: string;

  // Brand / interactive accent
  accent: string;
  accentText: string;
  link: string;

  // Semantic status
  danger: string;
  success: string;
  warning: string;
  info: string;
  merged: string;

  // Diff
  diffAdd: string;
  diffRemove: string;

  // Legacy aliases — existing `ThemedText`/`ThemedView` callers reference these
  // keys; kept mapped onto the ramp so no caller had to change in this pass.
  background: string;
  backgroundElement: string;
  backgroundSelected: string;
};

export const Colors = {
  light: {
    surface0: '#ffffff',
    surface1: '#F7F7F9',
    surface2: '#F0F0F3',
    surface3: '#E0E1E6',

    text: '#000000',
    textSecondary: '#60646C',
    textMuted: '#8A8F98',

    border: '#E4E4E7',
    borderStrong: '#D4D4D8',

    accent: '#0969DA',
    accentText: '#ffffff',
    link: '#0969DA',

    danger: '#D1372D', // warm red, darkened so it reads on white without screaming
    success: '#1F883D', // green-700 range — readable on light surfaces
    warning: '#BF8700', // amber darkened for contrast on white
    info: '#9A6700',
    merged: '#8250DF',

    diffAdd: '#1F883D',
    diffRemove: '#D1372D',

    background: '#ffffff',
    backgroundElement: '#F0F0F3',
    backgroundSelected: '#E0E1E6',
  },
  dark: {
    surface0: '#000000',
    surface1: '#161618',
    surface2: '#212225',
    surface3: '#2E3135',

    text: '#ffffff',
    textSecondary: '#B0B4BA',
    textMuted: '#8A8F98',

    border: '#2A2B2E',
    borderStrong: '#3A3C40',

    accent: '#3c87f7',
    accentText: '#ffffff',
    link: '#3c87f7',

    danger: '#F85149', // bright values proven on a black background
    success: '#3FB950',
    warning: '#F5A623',
    info: '#D29922',
    merged: '#A371F7',

    diffAdd: '#3FB950',
    diffRemove: '#F85149',

    background: '#000000',
    backgroundElement: '#212225',
    backgroundSelected: '#2E3135',
  },
} satisfies Record<'light' | 'dark', SchemeColors>;

export type ThemeColor = keyof SchemeColors;

export const Fonts = Platform.select({
  ios: {
    /** iOS `UIFontDescriptorSystemDesignDefault` */
    sans: 'system-ui',
    /** iOS `UIFontDescriptorSystemDesignSerif` */
    serif: 'ui-serif',
    /** iOS `UIFontDescriptorSystemDesignRounded` */
    rounded: 'ui-rounded',
    /** iOS `UIFontDescriptorSystemDesignMonospaced` */
    mono: 'ui-monospace',
  },
  default: {
    sans: 'normal',
    serif: 'serif',
    rounded: 'normal',
    mono: 'monospace',
  },
  web: {
    sans: 'var(--font-display)',
    serif: 'var(--font-serif)',
    rounded: 'var(--font-rounded)',
    mono: 'var(--font-mono)',
  },
});

export const Spacing = {
  half: 2,
  one: 4,
  two: 8,
  three: 16,
  four: 24,
  five: 32,
  six: 64,
} as const;

/** Corner radii. `full` pills anything. */
export const Radius = {
  none: 0,
  sm: 4,
  md: 8,
  lg: 12,
  xl: 16,
  full: 9999,
} as const;

export const FontSize = {
  xs: 12,
  sm: 14,
  base: 16,
  lg: 18,
  xl: 20,
  '2xl': 24,
  '3xl': 32,
} as const;

export const FontWeight = {
  normal: '400',
  medium: '500',
  semibold: '600',
  bold: '700',
} as const;

export const IconSize = {
  xs: 12,
  sm: 14,
  md: 16,
  lg: 20,
  xl: 24,
} as const;

/**
 * Elevation. Light and dark use different alpha maths on purpose: a shadow tuned
 * for white is invisible on black and vice-versa. Consumed by the primitive
 * components (cards, sheets) rather than screens directly.
 */
export const Shadows = {
  light: {
    sm: {
      shadowColor: '#000000',
      shadowOpacity: 0.06,
      shadowOffset: { width: 0, height: 1 },
      shadowRadius: 3,
      elevation: 1,
    },
    md: {
      shadowColor: '#000000',
      shadowOpacity: 0.1,
      shadowOffset: { width: 0, height: 4 },
      shadowRadius: 12,
      elevation: 4,
    },
  },
  dark: {
    sm: {
      shadowColor: '#000000',
      shadowOpacity: 0.4,
      shadowOffset: { width: 0, height: 1 },
      shadowRadius: 3,
      elevation: 1,
    },
    md: {
      shadowColor: '#000000',
      shadowOpacity: 0.55,
      shadowOffset: { width: 0, height: 6 },
      shadowRadius: 16,
      elevation: 8,
    },
  },
} as const;

export const BottomTabInset = Platform.select({ ios: 50, android: 80 }) ?? 0;
export const MaxContentWidth = 800;
