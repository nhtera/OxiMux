# Syntax highlighting

How OxiMux colors code, and — more importantly — the two decisions behind it
that look wrong until you know why.

## The discipline

> Do not add language-specific parsing to a renderer. Unknown languages,
> binaries, oversized sources, incompatible grammars and parse failures must
> remain **plain**. Highlighting changes foreground color **only** — never font,
> weight, style, wrapping, height, or scroll geometry.

The last clause is load-bearing. Because highlighting cannot change layout, it
can be computed lazily, off the paint path, and arrive late without reflowing
anything. Every degradation path in `crates/syntax` returns plain rather than an
error for the same reason.

## Decision 1 — the diff view cannot use tree-sitter

Recorded 2026-08-19, after investigating whether the diff view could move off
syntect.

Tree-sitter parses **whole documents**. A diff is **hunk excerpts**. Bridging
that means fetching the complete old and new sources, binding them to a
checksum, and discarding the result atomically on any mismatch. That is a real
architecture, and it does not fit here:

| Obstacle | Evidence |
|---|---|
| The diff model carries no whole document and no checksum | `crates/core/src/git_diff.rs:72` — `FileDiff` is `hunks: Vec<DiffHunk>` |
| Unstaged diffs compare against the **working tree** — the file the user is editing | the stale-checksum case, and here it is the common case, not the edge |
| Untracked files are not in git at all | nothing for `read_blob_at` to return |
| Remote-served diffs carry only hunks | `crates/remote-proto/src/messages.rs:459` — adding whole sources means a payload change inside an existing reply, which postcard's append-only rule makes unsafe, and would ship entire file contents over the remote link |
| The cost inverts | today only the lines shown are tokenized; whole-document parsing means parsing a 10,000-line file to color a 50-line hunk |

`read_blob_at` (`crates/git/src/repository.rs:521`) does exist, so *committed*
sides are obtainable. It is the other three rows that decide it.

**Conclusion: syntect stays.** `two-face`, the `[profile.dev.package.*]`
opt-level entries in the root manifest, and the diff view's per-line
`ParseState` path all remain. This was a pre-decided branch, not a failure.

## Decision 2 — `oximux-syntax` is built on syntect, not tree-sitter

Follows from the first, and inverts the original plan.

The goal was **one** highlighter for diff *and* chat, emitting neutral kinds.
Once diffs cannot move to tree-sitter, a tree-sitter crate would have left
syntect in place for diffs and added a **third** highlighter (syntect for diffs +
the new crate for chat + gpui-component's for the editor) — strictly worse than
the two we started with.

The insight that resolves it: **neutrality was never a tree-sitter property.**
syntect's `ParseState` yields TextMate scope names with no theme involved —
`entity.name.function.rust`, `string.quoted.double.rust`,
`comment.line.double-slash.rust`. Verified by spike before committing to the
design. `crates/syntax/src/kinds.rs` maps those to a small closed
`HighlightKind` set.

What the old diff path actually did wrong was copy literal sRGB out of a bundled
`.tmTheme` into every token. That made a color change a **re-tokenization**, and
tied the palette to a theme file rather than to OxiMux's own theme. The engine
was never the defect.

So syntect it is, which also keeps ~250 grammars where tree-sitter needs a
pinned crate plus a `highlights.scm` per language.

**What this does not claim.** syntect is not removed and neither is the dev
opt-level hack; this work makes the *kinds* neutral, not the dependency count
smaller. Migrating the diff view onto `oximux-syntax` is now unblocked — it would
give one code path for both surfaces and put diff colors under the app theme —
but it is not done, and is not counted as delivered.

## The crate

`crates/syntax`, dependencies: `syntect` + `two-face`. No UI toolkit, enforced by
a `cargo tree` gate in CI rather than by convention.

- `detect(path, fence_lang, source)` — fence tag, then exact filename, then
  extension, then an unambiguous shebang. `None` means plain, and is the normal
  answer for a fence tagged `text` or nothing at all.
- `highlight(lang, source)` — whole source, carrying grammar state across lines,
  so a multi-line string or block comment colors past its first line. For
  anything holding complete text: a chat fence, a file preview.
- `highlight_line(lang, line)` — one line standing alone, no state in or out. For
  a caller holding fragments, which is what a diff row is. A multi-line construct
  colors only as far as the line itself reveals — the honest answer when the
  surrounding lines genuinely are not present.
- `HighlightCache` — LRU, keyed by content (not position: a streaming block
  rewrites its own text every token, and a positional key would miss every
  repaint), bounded by entry count and retained spans. The key folds in
  `GRAMMAR_GENERATION`, so entries computed under an older mapping cannot be
  served after an upgrade.

### Known detection gaps

Asserted in tests so they are documented facts rather than surprises. None is a
regression — the diff view resolves against the same collection and shares them.

- `.jsx` and `.mjs` are not registered extensions; they render plain. Fixing this
  means an alias table onto the JavaScript grammar — a product-coverage decision,
  not a bug.
- `.h` resolves to Objective-C, not C. Telling them apart needs the file
  contents, and guessing wrong is worse than a consistent answer.

### Attribution

`two-face` bundles Sublime grammars under their own licenses.
`two_face::acknowledgement` exposes the attribution listing if an about/licenses
screen ever needs one.
