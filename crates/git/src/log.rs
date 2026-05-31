//! `git log` wrapper backing the commit-graph panel.
//!
//! Uses `-z` so records are NUL-terminated, paired with a unit-separator
//! (`\x1f`) between fields. NUL records let `%b` (commit body) carry its
//! own newlines without colliding with the record boundary, and `\x1f` is
//! a control byte git never emits in subjects/authors/dates so the field
//! split is unambiguous. GitCmd already forces `LANG=C` so the date
//! formatter stays locale-stable.

use crate::error::{GitError, Result};
use crate::process::GitCmd;
use crate::repository::Repository;
use oximux_core::{CommitInfo, RefLabel};
use std::time::Duration;

const LOG_TIMEOUT: Duration = Duration::from_secs(15);

/// Field separator (US, 0x1f). Records are NUL-terminated via `-z`, so this
/// separator only has to be absent from git's own field outputs — which it
/// always is in practice for `%H` / `%h` / `%s` / `%an` / `%ad` / `%D`.
/// `%b` is last, so any rogue `\x1f` inside a body wouldn't bleed into the
/// next field anyway.
///
/// `%D` is the unwrapped decorate field (no surrounding ` (…)`), comma-
/// separated when multiple refs decorate one commit. Empty for commits
/// with no decorations — the parser's empty-string check yields an empty
/// `Vec<RefLabel>` for those.
const LOG_FORMAT: &str = "%H%x1f%h%x1f%s%x1f%an%x1f%ad%x1f%D%x1f%b";

/// Compact author-date format ("May 20") that drops the year for the recent
/// view. Paired with `LOG_FORMAT`'s `%ad` so the date column stays narrow.
const LOG_DATE_FORMAT: &str = "--date=format-local:%b %d";

/// Field count produced by `LOG_FORMAT` — guards the parser against silent
/// drift if someone tweaks the format string without updating the splitter.
const LOG_FIELD_COUNT: usize = 7;

impl Repository {
    /// Last `max` commits on HEAD. Returns an empty vec on an initial repo
    /// with no commits yet (git exits 0 with empty stdout).
    pub async fn log_recent(&self, max: u32) -> Result<Vec<CommitInfo>> {
        self.run_log(&[format!("--max-count={max}")]).await
    }

    /// Same as `log_recent` but skips the first `offset` commits — backing
    /// "Load more" in the graph panel.
    pub async fn log_page(&self, offset: u32, max: u32) -> Result<Vec<CommitInfo>> {
        self.run_log(&[format!("--skip={offset}"), format!("--max-count={max}")])
            .await
    }

    async fn run_log(&self, extra: &[String]) -> Result<Vec<CommitInfo>> {
        let format_arg = format!("--pretty=format:{LOG_FORMAT}");
        let mut args: Vec<&str> = vec!["log", "-z"];
        for a in extra {
            args.push(a.as_str());
        }
        args.push(&format_arg);
        args.push(LOG_DATE_FORMAT);
        let out = GitCmd::new(self.workdir())
            .args(args)
            .timeout(LOG_TIMEOUT)
            .run()
            .await?;
        let s = std::str::from_utf8(&out.stdout)
            .map_err(|e| GitError::parse(format!("log stdout not utf-8: {e}")))?;
        Ok(parse_log_output(s))
    }
}

/// Split the raw `git log -z` output into commit records. Empty trailing
/// records (git emits no terminator after the last commit but a stray empty
/// segment can appear on edge cases) are dropped.
pub fn parse_log_output(out: &str) -> Vec<CommitInfo> {
    out.split('\0').filter_map(parse_log_record).collect()
}

/// Parse a single NUL-delimited record into a `CommitInfo`. Returns `None`
/// for malformed records (too few fields, empty OID) so the caller can drop
/// them silently without failing the whole list.
pub fn parse_log_record(record: &str) -> Option<CommitInfo> {
    if record.is_empty() {
        return None;
    }
    let mut parts = record.splitn(LOG_FIELD_COUNT, '\x1f');
    let oid = parts.next()?.to_string();
    let short_oid = parts.next()?.to_string();
    let subject = parts.next()?.to_string();
    let author = parts.next()?.to_string();
    let short_date = parts.next()?.to_string();
    // Decorate field (`%D`). Comma-separated when multiple refs
    // attach. Empty for commits with no decorations — yields empty
    // `Vec<RefLabel>` rather than `None` so the consumer doesn't need
    // a separate optionality check.
    let decorate = parts.next().unwrap_or("");
    let refs = parse_decorate(decorate);
    // Body is optional. `%b` may emit a trailing newline pair that we trim
    // so the tooltip doesn't render dead whitespace.
    let body = parts.next().unwrap_or("").trim_end().to_string();
    if oid.is_empty() || short_oid.is_empty() {
        return None;
    }
    Some(CommitInfo {
        oid,
        short_oid,
        subject,
        author,
        short_date,
        body,
        refs,
    })
}

/// Parse git's `%D` decorate field into structured `RefLabel`s.
///
/// `%D` emits a comma-and-space-separated list with the same content
/// `git log --decorate` would render between parens — examples:
///
/// - `"HEAD -> main, tag: v1.2.3, origin/main"` (HEAD attached to main, tagged)
/// - `"HEAD, tag: v1.0"` (detached HEAD on a tagged commit)
/// - `"origin/feature/x"` (remote-tracking only)
/// - `""` (no decorations)
///
/// The `HEAD -> branch` arrow expands into TWO entries in emission
/// order — `Head` followed by `BranchTip { name: "branch" }` — so the
/// UI can choose to render them as separate chips or collapse into a
/// combined "HEAD on branch" label. Detached HEAD emits a bare `Head`.
///
/// `tag:` prefix is the git convention for annotated and lightweight
/// tags. Remote-tracking branches are identified by a leading
/// `<remote>/` segment where `<remote>` matches a configured remote;
/// since we don't query `git remote` here, the heuristic is: a label
/// that contains `/` and isn't prefixed with `tag:` is treated as a
/// remote-tracking branch. The local-branch-with-slash case (`feature/
/// auth`) is the genuine ambiguity — `git log %D` doesn't distinguish.
/// We prefer the remote interpretation because local branches are far
/// more often shown via `HEAD -> ...`, and the misclassification only
/// affects chip color, not behavior.
///
/// Anything that doesn't match the above (e.g. `refs/stash`,
/// `refs/notes/commits`) lands as `Other { raw }` so the chip can
/// still render with the literal text and neutral styling.
pub fn parse_decorate(field: &str) -> Vec<RefLabel> {
    let trimmed = field.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for raw_part in trimmed.split(',') {
        let part = raw_part.trim();
        if part.is_empty() {
            continue;
        }
        // "HEAD -> main" → Head + BranchTip { main }
        if let Some(branch) = part.strip_prefix("HEAD -> ") {
            out.push(RefLabel::Head);
            out.push(RefLabel::BranchTip {
                name: branch.trim().to_string(),
            });
            continue;
        }
        // Bare "HEAD" (detached).
        if part == "HEAD" {
            out.push(RefLabel::Head);
            continue;
        }
        // "tag: vX.Y" → Tag.
        if let Some(name) = part.strip_prefix("tag: ") {
            out.push(RefLabel::Tag {
                name: name.trim().to_string(),
            });
            continue;
        }
        // Heuristic: contains '/' → remote-tracking branch. The
        // ambiguity with local `feature/auth`-style names is documented
        // in the function header.
        if part.contains('/') {
            out.push(RefLabel::RemoteBranch {
                name: part.to_string(),
            });
            continue;
        }
        // Local branch — bare label, no prefix, no slash.
        out.push(RefLabel::BranchTip {
            name: part.to_string(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(fields: &[&str]) -> String {
        fields.join("\x1f")
    }

    #[test]
    fn parses_normal_record_without_body() {
        let r = record(&[
            "c62d24a1234567890abcdef1234567890abcdef0",
            "c62d24a",
            "docs: hello world",
            "Alice",
            "May 20",
            "", // decorate
            "", // body
        ]);
        let info = parse_log_record(&r).unwrap();
        assert_eq!(info.oid, "c62d24a1234567890abcdef1234567890abcdef0");
        assert_eq!(info.short_oid, "c62d24a");
        assert_eq!(info.subject, "docs: hello world");
        assert_eq!(info.author, "Alice");
        assert_eq!(info.short_date, "May 20");
        assert_eq!(info.body, "");
        assert!(info.refs.is_empty());
    }

    #[test]
    fn parses_record_with_multiline_body() {
        let r = record(&[
            "abc1234567890abcdef1234567890abcdef000000",
            "abc1234",
            "feat: do the thing",
            "Bob",
            "May 19",
            "",
            "Adds the thing.\n\n- bullet one\n- bullet two\n",
        ]);
        let info = parse_log_record(&r).unwrap();
        assert_eq!(info.subject, "feat: do the thing");
        assert_eq!(info.body, "Adds the thing.\n\n- bullet one\n- bullet two");
    }

    #[test]
    fn subject_with_punctuation_is_preserved() {
        let r = record(&[
            "abc1234567890abcdef1234567890abcdef000000",
            "abc1234",
            "fix: cope with foo, bar; baz",
            "Bob",
            "May 19",
            "",
            "",
        ]);
        let info = parse_log_record(&r).unwrap();
        assert_eq!(info.subject, "fix: cope with foo, bar; baz");
    }

    #[test]
    fn rejects_missing_fields() {
        assert!(parse_log_record("only one field").is_none());
        assert!(parse_log_record("a\x1fb\x1fc\x1fd").is_none());
        assert!(parse_log_record("").is_none());
    }

    #[test]
    fn rejects_empty_oid() {
        let r = record(&["", "", "subject", "author", "date", "", ""]);
        assert!(parse_log_record(&r).is_none());
    }

    #[test]
    fn empty_subject_is_allowed() {
        let r = record(&[
            "abc1234567890abcdef1234567890abcdef000000",
            "abc1234",
            "",
            "Alice",
            "May 18",
            "",
            "",
        ]);
        let info = parse_log_record(&r).unwrap();
        assert_eq!(info.subject, "");
    }

    #[test]
    fn parse_log_output_splits_on_nul() {
        let r1 = record(&[
            "11112222333344445555666677778888999900aa",
            "1111222",
            "subj one",
            "Alice",
            "May 20",
            "",
            "body one",
        ]);
        let r2 = record(&[
            "bbbbccccddddeeeeffff00001111222233334444",
            "bbbbccc",
            "subj two",
            "Bob",
            "May 19",
            "",
            "",
        ]);
        let out = format!("{r1}\0{r2}");
        let parsed = parse_log_output(&out);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].subject, "subj one");
        assert_eq!(parsed[0].body, "body one");
        assert_eq!(parsed[1].subject, "subj two");
        assert_eq!(parsed[1].body, "");
    }

    // ---------- decorate parsing ----------

    #[test]
    fn decorate_empty_string_yields_no_refs() {
        assert!(parse_decorate("").is_empty());
        assert!(parse_decorate("   ").is_empty());
    }

    #[test]
    fn decorate_head_arrow_branch_expands_to_two_entries() {
        let refs = parse_decorate("HEAD -> main");
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0], RefLabel::Head);
        assert_eq!(
            refs[1],
            RefLabel::BranchTip {
                name: "main".to_string()
            }
        );
    }

    #[test]
    fn decorate_detached_head_is_bare() {
        let refs = parse_decorate("HEAD");
        assert_eq!(refs, vec![RefLabel::Head]);
    }

    #[test]
    fn decorate_tag_prefix() {
        let refs = parse_decorate("tag: v1.2.3");
        assert_eq!(
            refs,
            vec![RefLabel::Tag {
                name: "v1.2.3".to_string()
            }]
        );
    }

    #[test]
    fn decorate_remote_branch_heuristic() {
        let refs = parse_decorate("origin/main");
        assert_eq!(
            refs,
            vec![RefLabel::RemoteBranch {
                name: "origin/main".to_string()
            }]
        );
    }

    #[test]
    fn decorate_local_branch_no_slash() {
        let refs = parse_decorate("feature-x");
        assert_eq!(
            refs,
            vec![RefLabel::BranchTip {
                name: "feature-x".to_string()
            }]
        );
    }

    #[test]
    fn decorate_combined_head_tag_remote() {
        let refs = parse_decorate("HEAD -> main, tag: v1.2.3, origin/main");
        assert_eq!(refs.len(), 4);
        assert_eq!(refs[0], RefLabel::Head);
        assert_eq!(
            refs[1],
            RefLabel::BranchTip {
                name: "main".to_string()
            }
        );
        assert_eq!(
            refs[2],
            RefLabel::Tag {
                name: "v1.2.3".to_string()
            }
        );
        assert_eq!(
            refs[3],
            RefLabel::RemoteBranch {
                name: "origin/main".to_string()
            }
        );
    }

    #[test]
    fn decorate_multiple_tags_on_one_commit() {
        let refs = parse_decorate("tag: v1.0, tag: stable");
        assert_eq!(
            refs,
            vec![
                RefLabel::Tag {
                    name: "v1.0".to_string()
                },
                RefLabel::Tag {
                    name: "stable".to_string()
                }
            ]
        );
    }

    #[test]
    fn decorate_skips_empty_parts_between_commas() {
        // Defensive parse of malformed input — git itself doesn't emit
        // back-to-back commas, but the parser shouldn't choke on a
        // hand-fed or future-format edit.
        let refs = parse_decorate("main, , tag: v1");
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn decorate_local_branch_with_slash_misclassifies_as_remote() {
        // Documented heuristic: a slash-containing name w/o `tag:` is
        // treated as a remote-tracking branch. `feature/auth` is the
        // genuine ambiguity (could be local). Accepting the
        // misclassification — it only affects chip color.
        let refs = parse_decorate("feature/auth");
        assert_eq!(
            refs,
            vec![RefLabel::RemoteBranch {
                name: "feature/auth".to_string()
            }]
        );
    }
}
