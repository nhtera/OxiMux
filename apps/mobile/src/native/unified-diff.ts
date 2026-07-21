import type { DiffLineKind, DiffStatus, FileDiff, DiffHunk } from '@/native/git';

/**
 * Parse a unified diff string into the same `FileDiff` shape the git RPC returns
 * pre-parsed from Rust.
 *
 * **Why this exists in TypeScript at all.** `GitDiff` arrives already structured
 * — the desktop parses it and the phone only `JSON.parse`s the result. But a
 * turn diff (`ThreadEntry::TurnDiff.diff`) crosses the wire as a raw unified-diff
 * *string*, and the Rust parser that handles the git case is not reachable from
 * here. The alternatives were a new FFI call (a protocol bump for a rendering
 * feature) or this. Unified diff is a stable, well-trodden format, so parsing it
 * here keeps the whole phase in the no-protocol-change bucket.
 *
 * The output feeds the existing `DiffView` unmodified — a turn diff and a working
 * -tree diff differ in *source*, not in *shape*, so rendering them through two
 * components would be duplication with no payoff.
 *
 * Deliberately lenient. This parses output from several agent backends, and a
 * header this does not recognise should cost that one file its hunks, never the
 * whole transcript entry. Anything unparseable yields an empty list rather than a
 * throw.
 */
export function parseUnifiedDiff(input: string): FileDiff[] {
  if (!input) return [];

  const files: FileDiff[] = [];
  let file: FileDiff | undefined;
  let hunk: DiffHunk | undefined;
  // Tracked separately from `file.status` because the `---`/`+++` pair that
  // reveals add-vs-delete arrives *after* the `diff --git` line that starts the
  // file, so the status cannot be settled at creation time.
  let sawDevNullOld = false;

  const finishHunk = () => {
    if (file && hunk) file.hunks.push(hunk);
    hunk = undefined;
  };
  const finishFile = () => {
    finishHunk();
    if (file) files.push(file);
    file = undefined;
    sawDevNullOld = false;
  };

  for (const line of input.split('\n')) {
    if (line.startsWith('diff --git ')) {
      finishFile();
      file = { path: pathFromGitHeader(line), status: 'Modified', hunks: [], large: false };
      continue;
    }

    // A bare diff with no `diff --git` preamble (some backends emit only
    // `---`/`+++`). Start a file so its hunks are not dropped on the floor.
    if (line.startsWith('--- ')) {
      if (!file) file = { path: '', status: 'Modified', hunks: [], large: false };
      finishHunk();
      sawDevNullOld = line.slice(4).trim() === '/dev/null';
      continue;
    }

    if (line.startsWith('+++ ')) {
      if (!file) file = { path: '', status: 'Modified', hunks: [], large: false };
      const target = line.slice(4).trim();
      if (target === '/dev/null') {
        file.status = 'Deleted';
      } else {
        if (sawDevNullOld) file.status = 'Added';
        if (!file.path) file.path = stripPrefix(target);
      }
      continue;
    }

    if (line.startsWith('@@')) {
      finishHunk();
      hunk = parseHunkHeader(line);
      // An unparseable hunk header is skipped rather than fatal — see the
      // leniency note above.
      continue;
    }

    if (line.startsWith('rename to ') && file) {
      file.path = stripPrefix(line.slice('rename to '.length).trim());
      continue;
    }

    if (line.startsWith('Binary files') && file) {
      file.status = 'Binary';
      continue;
    }

    if (!hunk) continue; // preamble noise: `index`, `new file mode`, etc.

    // `\ No newline at end of file`
    if (line.startsWith('\\')) {
      hunk.lines.push({ kind: 'NoNewlineHint', content: '' });
      continue;
    }

    const kind = lineKind(line);
    if (!kind) continue;
    // The marker is stripped here because `DiffLine.content` is defined without
    // it — `DiffView` re-adds the +/- when it renders, so leaving it on would
    // double it.
    hunk.lines.push({ kind, content: line.slice(1) });
  }

  finishFile();
  return files;
}

function lineKind(line: string): DiffLineKind | undefined {
  switch (line[0]) {
    case '+':
      return 'Added';
    case '-':
      return 'Removed';
    case ' ':
      return 'Context';
    case undefined:
      // A genuinely empty line inside a hunk is a context line whose single
      // leading space was stripped by a trailing-whitespace-trimming producer.
      // Treating it as context keeps the hunk's line accounting intact.
      return 'Context';
    default:
      return undefined;
  }
}

/** `@@ -12,7 +12,9 @@ optional trailing context` */
function parseHunkHeader(line: string): DiffHunk | undefined {
  const m = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(.*)$/.exec(line);
  if (!m) return undefined;
  return {
    old_start: Number(m[1]),
    // A range written without a count means exactly one line, per the format.
    old_lines: m[2] === undefined ? 1 : Number(m[2]),
    new_start: Number(m[3]),
    new_lines: m[4] === undefined ? 1 : Number(m[4]),
    header_suffix: (m[5] ?? '').trim(),
    lines: [],
  };
}

/**
 * The post-image path from a `diff --git a/x b/y` line.
 *
 * Takes the `b/` side because that is the file as it now stands — which is what
 * a rename should be listed under. Splitting on ` b/` rather than on whitespace
 * is deliberate: paths may contain spaces.
 */
function pathFromGitHeader(line: string): string {
  const rest = line.slice('diff --git '.length);
  const at = rest.lastIndexOf(' b/');
  if (at === -1) return stripPrefix(rest.trim());
  return rest.slice(at + 3);
}

/** Drop a leading `a/` or `b/` git prefix. */
function stripPrefix(path: string): string {
  if (path.startsWith('a/') || path.startsWith('b/')) return path.slice(2);
  return path;
}

/** Added/removed line counts for a parsed file, for a stats header. */
export function countChanges(file: FileDiff): { added: number; removed: number } {
  let added = 0;
  let removed = 0;
  for (const hunk of file.hunks) {
    for (const line of hunk.lines) {
      if (line.kind === 'Added') added += 1;
      else if (line.kind === 'Removed') removed += 1;
    }
  }
  return { added, removed };
}

export type { DiffStatus };
