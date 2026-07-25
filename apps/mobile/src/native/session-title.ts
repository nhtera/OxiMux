/**
 * The host folds a session's project into the wire `title` as
 * `"<project> · <title>"` (the append-only remote protocol has no room for a
 * separate project field). Split it back so a session row can render the project
 * as a muted context label above the title.
 *
 * A title with no separator is returned unchanged with no project — that is what
 * an older host (which does not fold) sends, so the row degrades to exactly its
 * previous single-title form rather than showing anything odd.
 */
const SEPARATOR = ' · ';

export function parseSessionTitle(title: string): { project?: string; label: string } {
  const at = title.indexOf(SEPARATOR);
  // `> 0`, not `>= 0`: a leading separator would leave an empty project, which is
  // no more informative than the raw title — treat it as un-folded.
  if (at <= 0) return { label: title };
  return { project: title.slice(0, at), label: title.slice(at + SEPARATOR.length) };
}
