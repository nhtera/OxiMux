/**
 * Shared control sizing. Every button / chip / input derives its height from one
 * of these three named steps rather than a per-component magic number, so
 * controls line up across screens.
 */
export const ControlHeight = {
  /** Dense inline controls (chips, small tags). */
  tight: 28,
  /** Default compact button. */
  compact: 32,
  /** Primary tap target — meets the 44pt touch-target floor. */
  field: 44,
} as const;

export type ControlSize = keyof typeof ControlHeight;

/** Extra tap area around small icon-only controls that render below 44pt. */
export const IconHitSlop = { top: 8, bottom: 8, left: 8, right: 8 } as const;
