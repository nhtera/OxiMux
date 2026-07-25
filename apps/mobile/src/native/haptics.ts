import * as Haptics from 'expo-haptics';

/**
 * A two-tier haptic vocabulary, used sparingly. Every call is fire-and-forget and
 * swallows its error: haptics are unavailable on the simulator and on some
 * devices, and a missing buzz must never break the interaction it accompanies.
 *
 * - `tick`  — "this is about to happen" (arming, selecting, toggling): a light selection tap.
 * - `impact` — "this is now happening" (send, commit, destructive confirm): a firmer thud.
 */
export function tick(): void {
  Haptics.selectionAsync().catch(() => {});
}

export function impact(): void {
  Haptics.impactAsync(Haptics.ImpactFeedbackStyle.Medium).catch(() => {});
}
