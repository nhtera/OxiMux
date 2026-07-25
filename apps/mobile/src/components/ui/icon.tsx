import { type LucideIcon } from 'lucide-react-native';

import { IconSize } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';

type Props = {
  /** A lucide icon component, e.g. `Plus` from `lucide-react-native`. */
  icon: LucideIcon;
  /** A token key (`md`) or an explicit pixel size. Defaults to `md` (16). */
  size?: keyof typeof IconSize | number;
  /** Overrides the default `text` colour. Pass a resolved theme colour. */
  color?: string;
  strokeWidth?: number;
};

/**
 * Wraps a lucide icon so callers pass a token size and get the theme's text
 * colour by default — no ad-hoc icon sizes or hardcoded colours at the call site.
 */
export function Icon({ icon: IconComponent, size = 'md', color, strokeWidth = 2 }: Props) {
  const theme = useTheme();
  const px = typeof size === 'number' ? size : IconSize[size];
  return <IconComponent size={px} color={color ?? theme.text} strokeWidth={strokeWidth} />;
}
