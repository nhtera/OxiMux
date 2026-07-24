import type { ProjectSummary, SessionSummary } from 'oximux-core';

import { parseSessionTitle } from './session-title';

/**
 * One project's row in the grouped session list: the project header (with its
 * host path, so the "+" can start a session there) and the sessions that belong
 * to it. `path` is absent only for the synthetic "Other" bucket — sessions whose
 * project the host did not fold, or which match no known project — which has no
 * compose affordance because there is no path to start a session in.
 */
export type SessionGroup = {
  key: string;
  name: string;
  path?: string;
  data: SessionSummary[];
};

/** The final path component — the label the host folds into a session's title. */
export function basename(path: string): string {
  const trimmed = path.replace(/\/+$/, '');
  const at = trimmed.lastIndexOf('/');
  return at >= 0 ? trimmed.slice(at + 1) : trimmed;
}

const OTHER_KEY = '__other__';

/**
 * Group sessions under the host's projects for the grouped list.
 *
 * The host folds each session's project (its cwd's basename) into the title, and
 * a project's basename is the join key — so a session lands under the project it
 * was started in without any per-session path on the wire. Every project is kept
 * (in the host's order) even with no sessions, so the "+" can start one there —
 * that is the whole point of the grouped view. Sessions matching no project fall
 * into a trailing "Other" bucket, included only when non-empty.
 *
 * With no projects (an older host, or none configured) the result is a single
 * pathless group holding every session, so the screen degrades to a flat list
 * rather than hiding sessions behind absent headers.
 */
export function groupSessionsByProject(
  projects: ProjectSummary[],
  sessions: SessionSummary[]
): SessionGroup[] {
  if (projects.length === 0) {
    return [{ key: OTHER_KEY, name: '', data: sessions }];
  }

  const groups: SessionGroup[] = projects.map((p) => ({
    key: p.path,
    name: p.name,
    path: p.path,
    data: [],
  }));
  // Index by both the project's basename (matches the folded label) and its
  // display name, so a renamed project still catches its sessions.
  const byKey = new Map<string, SessionGroup>();
  for (const g of groups) {
    byKey.set(basename(g.path as string), g);
    byKey.set(g.name, g);
  }

  const other: SessionGroup = { key: OTHER_KEY, name: 'Other', data: [] };
  for (const session of sessions) {
    const { project } = parseSessionTitle(session.title);
    const target = (project && byKey.get(project)) || other;
    target.data.push(session);
  }

  return other.data.length > 0 ? [...groups, other] : groups;
}
