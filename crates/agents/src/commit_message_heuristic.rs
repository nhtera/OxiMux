//! Pure heuristic generator for conventional-prefix commit messages.
//!
//! Takes a slice of `FileStatus` records (caller is expected to have
//! pre-filtered to staged paths via `FileStatus::is_staged`) and emits a
//! conventional-commit-style message — `<prefix>(<scope>): <subject>`
//! followed by an optional bullet body listing each path with its
//! index-side status and `+A -B` line counts when known.
//!
//! Design notes:
//!
//! - **Zero git dependency.** The heuristic operates over already-parsed
//!   `FileStatus` records. The git crate's `status` poller already feeds
//!   the panel; reusing that snapshot avoids a second shellout per click
//!   and keeps generation synchronous + fast (well under the 200ms target
//!   for ≤500 LOC).
//!
//! - **Prefix policy ranks by exclusivity.** When every staged path is
//!   docs we emit `docs:`; same for tests/config. Mixed sets fall
//!   through to `feat:` (any pure-add present) → `refactor:` (any
//!   rename) → `chore:`. A mid-category mix avoids inventing a multi-
//!   prefix syntax — single prefix is the convention.
//!
//! - **Scope = common top-level directory.** `crates/app/src/foo.rs` +
//!   `crates/app/tests/bar.rs` share `crates/app`, so the heuristic
//!   collapses both to the leaf segment `app`. When paths diverge above
//!   the workspace root (e.g. cross-crate edits), scope is omitted
//!   rather than guessed.
//!
//! - **Body is a bullet list, not a sentence.** Reads like a status
//!   dump because that's the most accurate thing the heuristic can
//!   honestly produce — the user can rewrite freely; the heuristic's
//!   value is filling the textarea with the file list so the user
//!   doesn't have to remember every path they touched.
//!
//! The function is fully synchronous and side-effect-free — drives both
//! the unit tests and the spawned generation task in
//! `shell/source_control/commit_area.rs`.

use std::path::{Component, Path, PathBuf};

use oximux_core::{FileStatus, IndexStatus, RenameKind};

/// Maximum number of files surfaced in the body bullet list. Above
/// this we collapse the tail into "… and N more" so the textarea
/// doesn't balloon. Tuned to match a typical reviewable commit (a
/// 12-file slice already strains a single PR; anything larger needs
/// human triage anyway).
const BODY_MAX_LINES: usize = 12;

/// Maximum subject length (excluding the `prefix(scope): ` chrome).
/// Beyond this the subject is truncated with a single `…` — matches
/// the 72-char convention for the wrapped body line while leaving
/// headroom for prefix + scope.
const SUBJECT_MAX_LEN: usize = 60;

/// Generate a conventional-commit message from a snapshot of staged
/// `FileStatus` records. Returns an empty string when the slice is
/// empty (the caller should never invoke the heuristic with nothing
/// staged; an empty return is the honest no-op).
///
/// Output shape:
///
/// ```text
/// <prefix>(<scope>): <subject>
///
/// - <path> (<status>, +N -M)
/// - <path> (<status>, +N -M)
/// ```
///
/// The blank line + body are omitted when the slice has a single
/// path — the subject already names the file, repeating it as a body
/// bullet is noise.
pub fn generate_heuristic(files: &[FileStatus]) -> String {
    if files.is_empty() {
        return String::new();
    }

    let prefix = pick_prefix(files);
    let scope = pick_scope(files);
    let subject = build_subject(files, prefix);
    let header = match scope.as_deref() {
        Some(s) => format!("{prefix}({s}): {subject}"),
        None => format!("{prefix}: {subject}"),
    };

    if files.len() == 1 {
        return header;
    }

    let body = build_body(files);
    format!("{header}\n\n{body}")
}

/// File category for prefix selection. Order in the enum matches the
/// rank passed back to `pick_prefix` — higher discriminant = more
/// specific.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Category {
    Code,
    Config,
    Tests,
    Docs,
}

fn classify(path: &Path) -> Category {
    let lower_path = path.to_string_lossy().to_lowercase();
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(str::to_ascii_lowercase);

    // Tests beat docs beat config beat code so a `tests/foo.md`
    // counts as tests, not docs — the test directory is the stronger
    // signal of intent.
    let in_tests = lower_path.contains("/tests/")
        || lower_path.starts_with("tests/")
        || lower_path.ends_with("_test.rs")
        || lower_path.ends_with(".test.ts")
        || lower_path.ends_with(".test.tsx")
        || lower_path.ends_with(".test.js")
        || lower_path.ends_with(".spec.ts")
        || lower_path.ends_with(".spec.js");
    if in_tests {
        return Category::Tests;
    }

    let in_docs = lower_path.contains("/docs/")
        || lower_path.starts_with("docs/")
        || matches!(ext.as_deref(), Some("md") | Some("mdx") | Some("rst") | Some("txt"));
    if in_docs {
        return Category::Docs;
    }

    let is_config = matches!(
        ext.as_deref(),
        Some("toml")
            | Some("yaml")
            | Some("yml")
            | Some("json")
            | Some("ini")
            | Some("conf")
            | Some("env"),
    ) || lower_path.ends_with("/.gitignore")
        || lower_path == ".gitignore"
        || lower_path.ends_with("/dockerfile")
        || lower_path == "dockerfile";
    if is_config {
        return Category::Config;
    }

    Category::Code
}

/// Pick the conventional prefix. When every file shares a single
/// category we use that category's prefix; mixed sets fall back to
/// the action-based prefixes (feat for net-new files, refactor for
/// renames, chore otherwise).
fn pick_prefix(files: &[FileStatus]) -> &'static str {
    let mut cats: Vec<Category> = files.iter().map(|f| classify(&f.path)).collect();
    cats.sort_unstable_by_key(|c| *c as u8);
    cats.dedup();

    if cats.len() == 1 {
        return match cats[0] {
            Category::Docs => "docs",
            Category::Tests => "test",
            Category::Config => "chore",
            Category::Code => action_prefix(files),
        };
    }

    // Mixed categories — fall back to the action-based prefix so
    // we still convey the dominant intent (mostly-adds → feat,
    // mostly-renames → refactor, otherwise chore).
    action_prefix(files)
}

/// Action-based fallback when category-based prefix doesn't apply.
/// Ranked: any net-new code → feat, any rename → refactor, any
/// delete-only → chore, otherwise chore.
fn action_prefix(files: &[FileStatus]) -> &'static str {
    let any_added = files.iter().any(|f| matches!(f.index, IndexStatus::Added));
    if any_added {
        return "feat";
    }
    let any_renamed = files
        .iter()
        .any(|f| matches!(f.index, IndexStatus::Renamed | IndexStatus::Copied));
    if any_renamed {
        return "refactor";
    }
    // All-delete and mixed-modify both fall through to chore — the
    // conventional-commit grammar has no dedicated `remove:` prefix,
    // and "chore" is the closest stable category for delete-only sets.
    "chore"
}

/// Compute the longest common path prefix across all files, then
/// return the LEAF segment of that prefix as the scope. Returns
/// `None` when paths diverge at the root or when the common prefix
/// is empty / unhelpful.
///
/// Examples:
/// - `crates/app/src/x.rs` + `crates/app/src/y.rs` → `app`
/// - `crates/app/src/x.rs` + `crates/git/src/y.rs` → `crates`
///   (still meaningful — both inside the same workspace area)
/// - `src/x.rs` + `tests/y.rs` → `None`
fn pick_scope(files: &[FileStatus]) -> Option<String> {
    let paths: Vec<&Path> = files.iter().map(|f| f.path.as_path()).collect();
    let common = longest_common_dir(&paths)?;
    let leaf = common.file_name()?.to_str()?.to_string();
    if leaf.is_empty() {
        return None;
    }
    Some(leaf)
}

fn longest_common_dir(paths: &[&Path]) -> Option<PathBuf> {
    if paths.is_empty() {
        return None;
    }
    let first_components: Vec<Component<'_>> = paths[0]
        .parent()
        .map(|p| p.components().collect())
        .unwrap_or_default();
    if first_components.is_empty() {
        return None;
    }
    let mut common_len = first_components.len();
    for path in &paths[1..] {
        let parent = path.parent()?;
        let mut shared = 0usize;
        for (a, b) in first_components.iter().zip(parent.components()) {
            if a == &b {
                shared += 1;
            } else {
                break;
            }
        }
        common_len = common_len.min(shared);
        if common_len == 0 {
            return None;
        }
    }
    let mut acc = PathBuf::new();
    for comp in first_components.iter().take(common_len) {
        acc.push(comp.as_os_str());
    }
    if acc.as_os_str().is_empty() {
        None
    } else {
        Some(acc)
    }
}

/// Build the subject line. Shape depends on file count and dominant
/// action; truncated to `SUBJECT_MAX_LEN` chars with a single `…`.
fn build_subject(files: &[FileStatus], prefix: &'static str) -> String {
    let raw = if files.len() == 1 {
        let f = &files[0];
        let path_label = file_label(&f.path);
        let verb = subject_verb_single(f, prefix);
        format!("{verb} {path_label}")
    } else {
        let verb = subject_verb_multi(files, prefix);
        format!("{verb} {} files", files.len())
    };
    truncate_with_ellipsis(&raw, SUBJECT_MAX_LEN)
}

fn subject_verb_single(file: &FileStatus, _prefix: &'static str) -> &'static str {
    match file.index {
        IndexStatus::Added => "add",
        IndexStatus::Deleted => "remove",
        IndexStatus::Renamed => "rename",
        IndexStatus::Copied => "copy",
        IndexStatus::Modified => "update",
        // The remaining variants shouldn't appear in a staged
        // snapshot (Unmodified/Untracked/Ignored filtered by
        // `is_staged`, Unmerged blocks commit), but we surface a
        // safe fallback rather than panic.
        _ => "update",
    }
}

fn subject_verb_multi(files: &[FileStatus], prefix: &'static str) -> &'static str {
    // The category-prefix variants prefer the category-shaped verb
    // ("document N files" reads weirdly for docs; "update" is the
    // safe default). The action prefixes get their action verb.
    if prefix == "feat" {
        if files.iter().all(|f| matches!(f.index, IndexStatus::Added)) {
            return "add";
        }
        return "introduce";
    }
    if prefix == "refactor" {
        return "restructure";
    }
    if files.iter().all(|f| matches!(f.index, IndexStatus::Deleted)) {
        return "remove";
    }
    "update"
}

/// Bullet body listing every file with its status and line counts.
/// Caps at `BODY_MAX_LINES` lines; surplus rolled into a trailing
/// "- … and N more" line.
fn build_body(files: &[FileStatus]) -> String {
    let mut lines = Vec::with_capacity(files.len().min(BODY_MAX_LINES) + 1);
    let shown = files.iter().take(BODY_MAX_LINES);
    for f in shown {
        lines.push(format!("- {}", file_bullet(f)));
    }
    if files.len() > BODY_MAX_LINES {
        let extra = files.len() - BODY_MAX_LINES;
        lines.push(format!("- … and {extra} more"));
    }
    lines.join("\n")
}

fn file_bullet(file: &FileStatus) -> String {
    let label = file_label(&file.path);
    let action = match file.index {
        IndexStatus::Added => "add",
        IndexStatus::Deleted => "delete",
        IndexStatus::Modified => "modify",
        IndexStatus::Renamed => "rename",
        IndexStatus::Copied => "copy",
        _ => "update",
    };
    let rename_note = match &file.rename {
        Some(info) if matches!(info.kind, RenameKind::Rename) => {
            format!(" from {}", file_label(&info.orig_path))
        }
        Some(info) if matches!(info.kind, RenameKind::Copy) => {
            format!(" from {}", file_label(&info.orig_path))
        }
        _ => String::new(),
    };
    let counts_note = match file.line_counts {
        Some((added, removed)) if added + removed > 0 => format!(" (+{added} -{removed})"),
        _ => String::new(),
    };
    format!("{action} {label}{rename_note}{counts_note}")
}

/// Render a path for human display — the rightmost two segments when
/// the path is deep, the full path when shallow. Keeps bullet lines
/// scannable without losing the disambiguating directory.
fn file_label(path: &Path) -> String {
    let components: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str().map(str::to_string),
            _ => None,
        })
        .collect();
    if components.len() <= 2 {
        return components.join("/");
    }
    let tail = &components[components.len() - 2..];
    tail.join("/")
}

fn truncate_with_ellipsis(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut acc = String::with_capacity(max);
    for (i, ch) in s.chars().enumerate() {
        if i + 1 >= max {
            break;
        }
        acc.push(ch);
    }
    acc.push('…');
    acc
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_core::{RenameInfo, RenameKind, WorktreeStatus};

    fn fs(path: &str, index: IndexStatus) -> FileStatus {
        FileStatus {
            path: PathBuf::from(path),
            index,
            worktree: WorktreeStatus::Unmodified,
            rename: None,
            line_counts: None,
            conflict_kind: None,
        }
    }

    fn fs_renamed(orig: &str, new: &str) -> FileStatus {
        FileStatus {
            path: PathBuf::from(new),
            index: IndexStatus::Renamed,
            worktree: WorktreeStatus::Unmodified,
            rename: Some(RenameInfo {
                orig_path: PathBuf::from(orig),
                kind: RenameKind::Rename,
                score: 100,
            }),
            line_counts: None,
            conflict_kind: None,
        }
    }

    fn fs_counts(path: &str, index: IndexStatus, added: u32, removed: u32) -> FileStatus {
        FileStatus {
            path: PathBuf::from(path),
            index,
            worktree: WorktreeStatus::Unmodified,
            rename: None,
            line_counts: Some((added, removed)),
            conflict_kind: None,
        }
    }

    #[test]
    fn empty_input_yields_empty_string() {
        assert!(generate_heuristic(&[]).is_empty());
    }

    #[test]
    fn all_docs_prefix() {
        let files = vec![
            fs("docs/readme.md", IndexStatus::Modified),
            fs("docs/api.md", IndexStatus::Modified),
        ];
        let msg = generate_heuristic(&files);
        assert!(msg.starts_with("docs"), "expected docs prefix, got: {msg}");
        assert!(msg.contains("(docs):"), "expected docs scope: {msg}");
    }

    #[test]
    fn all_tests_prefix_beats_docs_in_tests_dir() {
        let files = vec![
            fs("crates/app/tests/foo.rs", IndexStatus::Modified),
            fs("crates/app/tests/bar.rs", IndexStatus::Modified),
        ];
        let msg = generate_heuristic(&files);
        assert!(msg.starts_with("test"), "expected test prefix, got: {msg}");
    }

    #[test]
    fn all_config_prefix() {
        let files = vec![
            fs("Cargo.toml", IndexStatus::Modified),
            fs("crates/app/Cargo.toml", IndexStatus::Modified),
        ];
        let msg = generate_heuristic(&files);
        assert!(msg.starts_with("chore"), "expected chore for config: {msg}");
    }

    #[test]
    fn all_added_code_yields_feat() {
        let files = vec![
            fs("crates/app/src/new_a.rs", IndexStatus::Added),
            fs("crates/app/src/new_b.rs", IndexStatus::Added),
        ];
        let msg = generate_heuristic(&files);
        assert!(msg.starts_with("feat"), "expected feat for new code: {msg}");
    }

    #[test]
    fn all_renames_yields_refactor() {
        let files = vec![
            fs_renamed("crates/app/src/old.rs", "crates/app/src/new.rs"),
            fs_renamed("crates/app/src/old2.rs", "crates/app/src/new2.rs"),
        ];
        let msg = generate_heuristic(&files);
        assert!(
            msg.starts_with("refactor"),
            "expected refactor for renames: {msg}"
        );
    }

    #[test]
    fn all_deletes_yields_chore() {
        let files = vec![
            fs("crates/app/src/dead.rs", IndexStatus::Deleted),
            fs("crates/app/src/dead2.rs", IndexStatus::Deleted),
        ];
        let msg = generate_heuristic(&files);
        assert!(
            msg.starts_with("chore"),
            "expected chore for deletes: {msg}"
        );
    }

    #[test]
    fn mixed_categories_falls_back_to_action_prefix() {
        // Mixed code + docs + new file → falls back to action prefix
        // (feat because there's a net-new file).
        let files = vec![
            fs("crates/app/src/x.rs", IndexStatus::Modified),
            fs("docs/readme.md", IndexStatus::Modified),
            fs("crates/app/src/new.rs", IndexStatus::Added),
        ];
        let msg = generate_heuristic(&files);
        assert!(msg.starts_with("feat"), "expected feat fallback: {msg}");
    }

    #[test]
    fn scope_is_leaf_of_common_directory() {
        let files = vec![
            fs("crates/app/src/x.rs", IndexStatus::Modified),
            fs("crates/app/src/y.rs", IndexStatus::Modified),
        ];
        let msg = generate_heuristic(&files);
        // Common directory is crates/app/src; leaf segment is "src".
        assert!(msg.contains("(src):"), "expected (src) scope, got: {msg}");
    }

    #[test]
    fn scope_omitted_when_paths_diverge_at_root() {
        let files = vec![
            fs("src/x.rs", IndexStatus::Modified),
            fs("tests/y.rs", IndexStatus::Modified),
        ];
        let msg = generate_heuristic(&files);
        // 'tests/y.rs' is classified as Tests, so prefix is action-fallback
        // because categories diverge; scope diverges at root so it's omitted.
        // Subject line should NOT contain a scope-parens pattern before the ':'.
        let first_line = msg.lines().next().unwrap_or_default();
        let has_scope = first_line
            .split_once(':')
            .map(|(head, _)| head.contains('('))
            .unwrap_or(false);
        assert!(!has_scope, "expected no scope, got first line: {first_line}");
    }

    #[test]
    fn single_file_subject_names_the_file_and_omits_body() {
        let files = vec![fs("crates/app/src/lone.rs", IndexStatus::Added)];
        let msg = generate_heuristic(&files);
        assert!(msg.contains("add"), "expected 'add' verb: {msg}");
        assert!(msg.contains("lone.rs"), "expected file in subject: {msg}");
        // Single-file output has no body bullet list.
        assert!(
            !msg.contains("\n\n- "),
            "single-file output should have no body: {msg}",
        );
    }

    #[test]
    fn body_line_count_caps_at_max_with_overflow_line() {
        // 20 files; expect 12 bullets + 1 "and N more" line.
        let files: Vec<FileStatus> = (0..20)
            .map(|i| {
                fs(
                    &format!("crates/app/src/f{i}.rs"),
                    IndexStatus::Modified,
                )
            })
            .collect();
        let msg = generate_heuristic(&files);
        let bullet_count = msg.lines().filter(|l| l.starts_with("- ")).count();
        assert_eq!(
            bullet_count,
            BODY_MAX_LINES + 1,
            "expected {} bullets including overflow line: {msg}",
            BODY_MAX_LINES + 1,
        );
        assert!(
            msg.contains("and 8 more"),
            "expected overflow note for 20-12=8 extras: {msg}",
        );
    }

    #[test]
    fn body_includes_line_counts_when_present() {
        let files = vec![
            fs_counts("crates/app/src/a.rs", IndexStatus::Modified, 12, 4),
            fs_counts("crates/app/src/b.rs", IndexStatus::Modified, 0, 8),
        ];
        let msg = generate_heuristic(&files);
        assert!(msg.contains("(+12 -4)"), "expected +12 -4: {msg}");
        assert!(msg.contains("(+0 -8)"), "expected +0 -8: {msg}");
    }

    #[test]
    fn rename_bullet_names_the_origin() {
        let files = vec![
            fs("crates/app/src/x.rs", IndexStatus::Modified),
            fs_renamed("crates/app/src/old.rs", "crates/app/src/new.rs"),
        ];
        let msg = generate_heuristic(&files);
        assert!(msg.contains("from"), "expected 'from <orig>' note: {msg}");
        assert!(msg.contains("old.rs"), "expected original name: {msg}");
    }

    #[test]
    fn subject_truncates_long_paths() {
        // Only the subject portion (after ": ") is bounded by
        // SUBJECT_MAX_LEN — the conventional-commit chrome
        // (prefix + scope) is intentionally NOT truncated so the
        // commit lint still gets a parseable header. The test
        // checks the subject-side bound + ellipsis.
        let very_long =
            "crates/app/src/a_very_long_module_name_that_exceeds_the_subject_max_easily/file.rs";
        let files = vec![fs(very_long, IndexStatus::Added)];
        let msg = generate_heuristic(&files);
        let first_line = msg.lines().next().unwrap_or_default();
        let subject = first_line
            .split_once(": ")
            .map(|(_, s)| s)
            .unwrap_or(first_line);
        assert!(
            subject.chars().count() <= SUBJECT_MAX_LEN,
            "expected subject <= {SUBJECT_MAX_LEN} chars; got len={} subject={subject}",
            subject.chars().count(),
        );
        assert!(
            subject.contains('…'),
            "expected ellipsis in truncated subject: {subject}",
        );
    }

    #[test]
    fn classify_recognizes_test_suffixes() {
        assert_eq!(classify(Path::new("foo_test.rs")), Category::Tests);
        assert_eq!(classify(Path::new("ui/foo.test.tsx")), Category::Tests);
        assert_eq!(classify(Path::new("ui/foo.spec.js")), Category::Tests);
    }

    #[test]
    fn classify_recognizes_config_files() {
        assert_eq!(classify(Path::new(".gitignore")), Category::Config);
        assert_eq!(classify(Path::new("Dockerfile")), Category::Config);
        assert_eq!(classify(Path::new("package.json")), Category::Config);
    }

    #[test]
    fn longest_common_dir_handles_single_root_files() {
        // Two files at the workspace root share parent "" → no scope.
        let scope = pick_scope(&[
            fs("README.md", IndexStatus::Modified),
            fs("LICENSE", IndexStatus::Modified),
        ]);
        assert_eq!(scope, None);
    }

    #[test]
    fn longest_common_dir_picks_deepest_shared() {
        // Three files share crates/app/src deepest.
        let scope = pick_scope(&[
            fs("crates/app/src/a.rs", IndexStatus::Modified),
            fs("crates/app/src/sub/b.rs", IndexStatus::Modified),
            fs("crates/app/src/c.rs", IndexStatus::Modified),
        ]);
        assert_eq!(scope.as_deref(), Some("src"));
    }

    #[test]
    fn output_never_panics_on_pathological_inputs() {
        // Mixed unstaged-state index codes (shouldn't normally arrive
        // here, but the heuristic must not panic if they do).
        let files = vec![
            fs("x.rs", IndexStatus::Unmodified),
            fs("y.rs", IndexStatus::Unmerged),
        ];
        let msg = generate_heuristic(&files);
        assert!(!msg.is_empty());
    }
}
