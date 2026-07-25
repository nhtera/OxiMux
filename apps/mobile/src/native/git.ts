/**
 * The git read model, mirroring `remote-proto`'s wire types.
 *
 * Like the transcript, status and diffs cross the FFI as canonical JSON rather
 * than as projected uniffi records, so these are the serde shapes verbatim:
 * snake_case fields, and externally-tagged enums (a unit variant is the bare
 * string `"Modified"`, a data-carrying one is `{ Renamed: { from, similarity } }`).
 */

export type IndexStatus =
  | 'Unmodified'
  | 'Modified'
  | 'Added'
  | 'Deleted'
  | 'Renamed'
  | 'Copied'
  | 'Untracked'
  | 'Ignored'
  | 'Unmerged';

export type WorktreeStatus =
  | 'Unmodified'
  | 'Modified'
  | 'Deleted'
  | 'Renamed'
  | 'Untracked'
  | 'Ignored'
  | 'Unmerged';

export type GitFile = {
  /** Repository-relative, as git reports it — echoed back verbatim on a diff. */
  path: string;
  index: IndexStatus;
  worktree: WorktreeStatus;
  /** `[added, removed]` line counts, when git produced them. */
  unstaged_lines: [number, number] | null;
  staged_lines: [number, number] | null;
};

export type GitStatus = {
  /** `null` when HEAD is detached. */
  branch: string | null;
  upstream: string | null;
  ahead: number;
  behind: number;
  files: GitFile[];
};

export type DiffStatus =
  | 'Added'
  | 'Modified'
  | 'Deleted'
  | 'Binary'
  | { Renamed: { from: string; similarity: number } }
  | { Copied: { from: string; similarity: number } }
  | { ModeChanged: { old_mode: number; new_mode: number } };

export type DiffLineKind = 'Context' | 'Added' | 'Removed' | 'NoNewlineHint';

export type DiffLine = {
  kind: DiffLineKind;
  /** Body text with the leading `+`/`-`/space marker already stripped. */
  content: string;
};

export type DiffHunk = {
  old_start: number;
  old_lines: number;
  new_start: number;
  new_lines: number;
  header_suffix: string;
  lines: DiffLine[];
};

export type FileDiff = {
  path: string;
  status: DiffStatus;
  hunks: DiffHunk[];
  /** Past the desktop's large-diff threshold. Hunks are still complete. */
  large: boolean;
};

/**
 * Parse a JSON payload from the git FFI.
 *
 * Best-effort by design: these shapes belong to the desktop and are free to
 * grow, so an unrecognised payload yields `undefined` rather than a throw that
 * would take down the screen.
 */
export function parseGit<T>(json: string): T | undefined {
  try {
    return JSON.parse(json) as T;
  } catch {
    return undefined;
  }
}

/** Whether a path has changes staged for commit. */
export function isStaged(file: GitFile): boolean {
  return file.index !== 'Unmodified' && file.index !== 'Untracked' && file.index !== 'Ignored';
}

/** Whether a path is untracked — the one case needing the read-off-disk diff. */
export function isUntracked(file: GitFile): boolean {
  return file.index === 'Untracked' || file.worktree === 'Untracked';
}

/** A short label for the row, preferring whichever side actually changed. */
export function fileLabel(file: GitFile): string {
  if (isUntracked(file)) return 'new';
  const side = isStaged(file) ? file.index : file.worktree;
  return side.toLowerCase();
}

/** `[added, removed]` for whichever side the row is showing. */
export function fileLines(file: GitFile): [number, number] {
  return (isStaged(file) ? file.staged_lines : file.unstaged_lines) ?? [0, 0];
}
