import { countChanges, parseUnifiedDiff } from '@/native/unified-diff';

describe('parseUnifiedDiff', () => {
  it('parses a modified file into hunks with markers stripped', () => {
    const diff = [
      'diff --git a/src/main.rs b/src/main.rs',
      'index 1234567..89abcde 100644',
      '--- a/src/main.rs',
      '+++ b/src/main.rs',
      '@@ -1,3 +1,4 @@ fn main()',
      ' let a = 1;',
      '-let b = 2;',
      '+let b = 3;',
      '+let c = 4;',
    ].join('\n');

    const [file] = parseUnifiedDiff(diff);
    expect(file.path).toBe('src/main.rs');
    expect(file.status).toBe('Modified');
    expect(file.hunks).toHaveLength(1);

    const hunk = file.hunks[0];
    expect(hunk.old_start).toBe(1);
    expect(hunk.old_lines).toBe(3);
    expect(hunk.new_start).toBe(1);
    expect(hunk.new_lines).toBe(4);
    expect(hunk.header_suffix).toBe('fn main()');

    // `DiffView` re-adds the +/- when rendering, so a marker left on here would
    // render doubled.
    expect(hunk.lines).toEqual([
      { kind: 'Context', content: 'let a = 1;' },
      { kind: 'Removed', content: 'let b = 2;' },
      { kind: 'Added', content: 'let b = 3;' },
      { kind: 'Added', content: 'let c = 4;' },
    ]);
  });

  it('detects an added file from a /dev/null old side', () => {
    const diff = [
      'diff --git a/new.txt b/new.txt',
      'new file mode 100644',
      '--- /dev/null',
      '+++ b/new.txt',
      '@@ -0,0 +1 @@',
      '+hello',
    ].join('\n');

    const [file] = parseUnifiedDiff(diff);
    expect(file.status).toBe('Added');
    expect(file.path).toBe('new.txt');
    // A range with no comma means exactly one line, per the format.
    expect(file.hunks[0].new_lines).toBe(1);
    expect(file.hunks[0].old_start).toBe(0);
  });

  it('detects a deleted file from a /dev/null new side', () => {
    const diff = [
      'diff --git a/gone.txt b/gone.txt',
      'deleted file mode 100644',
      '--- a/gone.txt',
      '+++ /dev/null',
      '@@ -1 +0,0 @@',
      '-bye',
    ].join('\n');

    expect(parseUnifiedDiff(diff)[0].status).toBe('Deleted');
  });

  it('separates multiple files', () => {
    const diff = [
      'diff --git a/one.txt b/one.txt',
      '--- a/one.txt',
      '+++ b/one.txt',
      '@@ -1 +1 @@',
      '-a',
      '+b',
      'diff --git a/two.txt b/two.txt',
      '--- a/two.txt',
      '+++ b/two.txt',
      '@@ -1 +1 @@',
      '-c',
      '+d',
    ].join('\n');

    const files = parseUnifiedDiff(diff);
    expect(files.map((f) => f.path)).toEqual(['one.txt', 'two.txt']);
    expect(files[0].hunks[0].lines).toHaveLength(2);
    expect(files[1].hunks[0].lines).toHaveLength(2);
  });

  it('takes the post-image path, so a rename lists under its new name', () => {
    const diff = [
      'diff --git a/old/name.ts b/new/name.ts',
      'similarity index 95%',
      'rename from old/name.ts',
      'rename to new/name.ts',
    ].join('\n');

    expect(parseUnifiedDiff(diff)[0].path).toBe('new/name.ts');
  });

  it('keeps a path containing spaces intact', () => {
    // Splitting the header on whitespace would truncate this; the parser splits
    // on ` b/` for exactly this reason.
    const diff = 'diff --git a/my docs/a file.md b/my docs/a file.md';
    expect(parseUnifiedDiff(diff)[0].path).toBe('my docs/a file.md');
  });

  it('records the no-newline hint', () => {
    const diff = [
      'diff --git a/x b/x',
      '--- a/x',
      '+++ b/x',
      '@@ -1 +1 @@',
      '-a',
      '+b',
      '\\ No newline at end of file',
    ].join('\n');

    const lines = parseUnifiedDiff(diff)[0].hunks[0].lines;
    expect(lines[lines.length - 1]).toEqual({ kind: 'NoNewlineHint', content: '' });
  });

  it('treats a bare empty line inside a hunk as context', () => {
    // A producer that trims trailing whitespace turns a blank context line's
    // single leading space into nothing. Dropping it would desync the hunk.
    const diff = ['--- a/x', '+++ b/x', '@@ -1,2 +1,2 @@', ' first', '', '+added'].join('\n');
    const lines = parseUnifiedDiff(diff)[0].hunks[0].lines;
    expect(lines).toContainEqual({ kind: 'Context', content: '' });
  });

  it('parses a diff that has no `diff --git` preamble', () => {
    const diff = ['--- a/plain.txt', '+++ b/plain.txt', '@@ -1 +1 @@', '-x', '+y'].join('\n');
    const [file] = parseUnifiedDiff(diff);
    expect(file.path).toBe('plain.txt');
    expect(file.hunks[0].lines).toHaveLength(2);
  });

  it('flags a binary file', () => {
    const diff = [
      'diff --git a/img.png b/img.png',
      'Binary files a/img.png and b/img.png differ',
    ].join('\n');
    expect(parseUnifiedDiff(diff)[0].status).toBe('Binary');
  });

  it.each([
    ['empty string', ''],
    ['unrelated prose', 'this is not a diff at all'],
    ['a truncated header', 'diff --git a/x b/x\n@@ this is not a hunk header'],
  ])('returns without throwing for %s', (_label, input) => {
    expect(() => parseUnifiedDiff(input)).not.toThrow();
  });

  it('drops an unparseable hunk header without losing the rest of the file', () => {
    const diff = [
      'diff --git a/x b/x',
      '@@ garbage @@',
      '+orphaned',
      '@@ -1 +1 @@',
      '+kept',
    ].join('\n');

    const hunks = parseUnifiedDiff(diff)[0].hunks;
    expect(hunks).toHaveLength(1);
    expect(hunks[0].lines).toEqual([{ kind: 'Added', content: 'kept' }]);
  });
});

describe('countChanges', () => {
  it('counts added and removed lines, ignoring context', () => {
    const diff = [
      'diff --git a/x b/x',
      '--- a/x',
      '+++ b/x',
      '@@ -1,3 +1,4 @@',
      ' ctx',
      '-one',
      '-two',
      '+three',
    ].join('\n');

    expect(countChanges(parseUnifiedDiff(diff)[0])).toEqual({ added: 1, removed: 2 });
  });
});
