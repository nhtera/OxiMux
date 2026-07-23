/**
 * Pure helpers for the recurrence picker, kept out of the component so the
 * label arithmetic is testable without rendering.
 *
 * These mirror the desktop's own `describe`/interval logic. They are the phone's
 * only copy of that phrasing that the wire does *not* carry: a stored schedule
 * arrives with a ready-made `summary` string, but the create form has no schedule
 * yet, so it labels the presets itself.
 */

/** Smallest interval the desktop accepts. Presets are checked against it. */
export const MIN_INTERVAL_MINUTES = 5;

/** The interval presets offered in the picker, matching the desktop's set. */
export const INTERVAL_CHOICES = [5, 10, 15, 30, 60, 120, 240, 480] as const;

/** `weekday` is 0=Monday, matching the wire and the desktop. */
export const WEEKDAYS = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'] as const;

/**
 * A natural label for an interval in minutes: whole hours read as hours, anything
 * else as minutes. "60 min" is technically correct but "1 hour" is what a person
 * means, so a whole-hour multiple collapses to hours.
 */
export function intervalLabel(minutes: number): string {
  if (minutes >= 60 && minutes % 60 === 0) {
    const hours = minutes / 60;
    return hours === 1 ? '1 hour' : `${hours} hours`;
  }
  return `${minutes} min`;
}
