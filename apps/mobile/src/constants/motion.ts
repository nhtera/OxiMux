import { Easing } from 'react-native-reanimated';

/**
 * One motion curve for every sliding surface — the nav drawer, bottom sheets, the
 * segmented-control thumb — so the whole app animates as one system rather than
 * each component inventing its own feel.
 *
 * A 220ms standard ease, matching the reference. Deliberately NOT a spring: an
 * underdamped spring overshot its target and bounced back, which read as the panel
 * "jumping" past open and settling. A monotonic ease never overshoots.
 */
export const MOTION_DURATION = 220;
export const MOTION_EASING = Easing.bezier(0.25, 0.1, 0.25, 1);
export const MOTION_TIMING = { duration: MOTION_DURATION, easing: MOTION_EASING } as const;
