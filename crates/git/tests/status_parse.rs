//! Fixture-driven unit tests for `parse_porcelain_v2`.
//!
//! Each fixture is the literal byte stream `git status --porcelain=v2 --branch -z`
//! would emit. NUL terminators between records are written as `\x00` in source.

use oximux_core::{ConflictKind, IndexStatus, RenameKind, WorktreeStatus};
use oximux_git::parse_porcelain_v2;

#[test]
fn empty_repo_initial_commit() {
    let buf = b"# branch.oid (initial)\x00# branch.head main\x00";
    let s = parse_porcelain_v2(buf).expect("parse");
    assert_eq!(s.branch.as_deref(), Some("main"));
    assert_eq!(s.head_oid, None);
    assert_eq!(s.upstream, None);
    assert_eq!(s.ahead, 0);
    assert_eq!(s.behind, 0);
    assert!(s.files.is_empty());
}

#[test]
fn clean_repo_with_ahead_behind() {
    let buf = b"\
# branch.oid abc123\x00\
# branch.head main\x00\
# branch.upstream origin/main\x00\
# branch.ab +2 -3\x00";
    let s = parse_porcelain_v2(buf).expect("parse");
    assert_eq!(s.branch.as_deref(), Some("main"));
    assert_eq!(s.head_oid.as_deref(), Some("abc123"));
    assert_eq!(s.upstream.as_deref(), Some("origin/main"));
    assert_eq!(s.ahead, 2);
    assert_eq!(s.behind, 3);
    assert!(s.files.is_empty());
}

#[test]
fn detached_head() {
    let buf = b"# branch.oid deadbeef\x00# branch.head (detached)\x00";
    let s = parse_porcelain_v2(buf).expect("parse");
    assert_eq!(s.branch, None);
    assert_eq!(s.head_oid.as_deref(), Some("deadbeef"));
}

#[test]
fn one_untracked_and_one_ignored() {
    let buf = b"\
# branch.head main\x00\
? new.txt\x00\
! ignored.log\x00";
    let s = parse_porcelain_v2(buf).expect("parse");
    assert_eq!(s.files.len(), 2);
    assert_eq!(s.files[0].path.to_str(), Some("new.txt"));
    assert!(matches!(s.files[0].index, IndexStatus::Untracked));
    assert_eq!(s.files[1].path.to_str(), Some("ignored.log"));
    assert!(matches!(s.files[1].index, IndexStatus::Ignored));
}

#[test]
fn ordinary_modified_unstaged() {
    // `M.` = modified in worktree, unmodified in index
    let buf = b"\
# branch.head main\x00\
1 .M N... 100644 100644 100644 hHash iHash src/lib.rs\x00";
    let s = parse_porcelain_v2(buf).expect("parse");
    assert_eq!(s.files.len(), 1);
    let f = &s.files[0];
    assert_eq!(f.path.to_str(), Some("src/lib.rs"));
    assert!(matches!(f.index, IndexStatus::Unmodified));
    assert!(matches!(f.worktree, WorktreeStatus::Modified));
    assert!(!f.is_staged());
    assert!(f.is_unstaged());
}

#[test]
fn ordinary_modified_staged() {
    let buf = b"\
# branch.head main\x00\
1 M. N... 100644 100644 100644 hHash iHash src/lib.rs\x00";
    let s = parse_porcelain_v2(buf).expect("parse");
    let f = &s.files[0];
    assert!(matches!(f.index, IndexStatus::Modified));
    assert!(matches!(f.worktree, WorktreeStatus::Unmodified));
    assert!(f.is_staged());
}

#[test]
fn renamed_file() {
    // R100 means rename with 100% similarity. The new path comes first then NUL
    // then the orig path. Whole record is NUL-terminated again at the end.
    let buf = b"\
# branch.head main\x00\
2 R. N... 100644 100644 100644 hHash iHash R100 new.txt\x00old.txt\x00";
    let s = parse_porcelain_v2(buf).expect("parse");
    assert_eq!(s.files.len(), 1);
    let f = &s.files[0];
    assert_eq!(f.path.to_str(), Some("new.txt"));
    assert!(matches!(f.index, IndexStatus::Renamed));
    let r = f.rename.as_ref().expect("rename info");
    assert_eq!(r.orig_path.to_str(), Some("old.txt"));
    assert!(matches!(r.kind, RenameKind::Rename));
    assert_eq!(r.score, 100);
}

#[test]
fn copied_file_partial_similarity() {
    let buf = b"\
# branch.head main\x00\
2 C. N... 100644 100644 100644 hHash iHash C75 copy.txt\x00orig.txt\x00";
    let s = parse_porcelain_v2(buf).expect("parse");
    let f = &s.files[0];
    let r = f.rename.as_ref().expect("rename info");
    assert!(matches!(r.kind, RenameKind::Copy));
    assert_eq!(r.score, 75);
}

#[test]
fn unmerged_file_populates_conflict_kind() {
    let buf = b"\
# branch.head main\x00\
u UU N... 100644 100644 100644 100644 hH1 hH2 hH3 conflict.txt\x00";
    let s = parse_porcelain_v2(buf).expect("parse");
    let f = &s.files[0];
    assert_eq!(f.path.to_str(), Some("conflict.txt"));
    assert!(matches!(f.index, IndexStatus::Unmerged));
    assert!(matches!(f.worktree, WorktreeStatus::Unmerged));
    assert_eq!(f.conflict_kind, Some(ConflictKind::BothModified));
}

#[test]
fn unmerged_kinds_all_seven_pairs() {
    // Every legal porcelain v2 u-record XY combination must classify.
    // Builds one fixture per (xy, kind) pair so a regression in `from_xy`
    // surfaces with a precise variant in the failure message.
    let cases: &[(&[u8; 2], ConflictKind)] = &[
        (b"DD", ConflictKind::BothDeleted),
        (b"AU", ConflictKind::AddedByUs),
        (b"UD", ConflictKind::DeletedByThem),
        (b"UA", ConflictKind::AddedByThem),
        (b"DU", ConflictKind::DeletedByUs),
        (b"AA", ConflictKind::BothAdded),
        (b"UU", ConflictKind::BothModified),
    ];
    for (xy, expected) in cases {
        let mut buf: Vec<u8> = b"# branch.head main\x00u ".to_vec();
        buf.extend_from_slice(*xy);
        buf.extend_from_slice(
            b" N... 100644 100644 100644 100644 hH1 hH2 hH3 conflict.txt\x00",
        );
        let s = parse_porcelain_v2(&buf).expect("parse");
        let f = &s.files[0];
        assert_eq!(
            f.conflict_kind,
            Some(*expected),
            "xy={:?} should map to {:?}",
            std::str::from_utf8(xy.as_slice()).unwrap(),
            expected
        );
    }
}

#[test]
fn ordinary_record_leaves_conflict_kind_none() {
    // A type-1 modified record must NOT carry a conflict_kind.
    let buf = b"\
# branch.head main\x00\
1 .M N... 100644 100644 100644 hH iH src.rs\x00";
    let s = parse_porcelain_v2(buf).expect("parse");
    assert_eq!(s.files[0].conflict_kind, None);
    assert_eq!(s.files[0].line_counts, None);
}

#[test]
fn path_with_spaces_works_under_z() {
    // The whole record is NUL-terminated, so spaces in filenames are fine.
    let buf = b"\
# branch.head main\x00\
1 .M N... 100644 100644 100644 hH iH spaced name.txt\x00";
    let s = parse_porcelain_v2(buf).expect("parse");
    assert_eq!(s.files[0].path.to_str(), Some("spaced name.txt"));
}

#[test]
fn mixed_repo() {
    let buf = b"\
# branch.oid abc123\x00\
# branch.head feature/x\x00\
# branch.upstream origin/feature/x\x00\
# branch.ab +1 -0\x00\
1 M. N... 100644 100644 100644 hH iH src/a.rs\x00\
1 .M N... 100644 100644 100644 hH iH src/b.rs\x00\
2 R. N... 100644 100644 100644 hH iH R90 src/renamed.rs\x00src/old.rs\x00\
u UU N... 100644 100644 100644 100644 h1 h2 h3 conflict.toml\x00\
? new.md\x00\
! tmp.log\x00";
    let s = parse_porcelain_v2(buf).expect("parse");
    assert_eq!(s.branch.as_deref(), Some("feature/x"));
    assert_eq!(s.ahead, 1);
    assert_eq!(s.behind, 0);
    assert_eq!(s.files.len(), 6);
    // ordering preserved from input
    assert_eq!(s.files[0].path.to_str(), Some("src/a.rs"));
    assert_eq!(s.files[1].path.to_str(), Some("src/b.rs"));
    assert_eq!(s.files[2].path.to_str(), Some("src/renamed.rs"));
    assert_eq!(s.files[3].path.to_str(), Some("conflict.toml"));
    assert_eq!(s.files[4].path.to_str(), Some("new.md"));
    assert_eq!(s.files[5].path.to_str(), Some("tmp.log"));
}

#[test]
fn malformed_xy_rejected() {
    let buf = b"# branch.head main\x001 X N... 100644 100644 100644 hH iH path\x00";
    let err = parse_porcelain_v2(buf).expect_err("malformed XY should fail");
    let msg = format!("{err}");
    assert!(msg.contains("XY") || msg.contains("unknown"), "got: {msg}");
}

#[test]
fn clean_repo_headers_only_no_files() {
    let buf = b"\
# branch.oid abc123\x00\
# branch.head main\x00\
# branch.upstream origin/main\x00\
# branch.ab +0 -0\x00";
    let s = parse_porcelain_v2(buf).expect("parse");
    assert!(s.files.is_empty());
    assert_eq!(s.ahead, 0);
    assert_eq!(s.behind, 0);
}

#[test]
fn double_space_in_record_rejected() {
    // Regression: prior parser tolerated extra spaces and silently shifted
    // field indices, corrupting XY status. Must error explicitly.
    let buf = b"\
# branch.head main\x00\
1 .M  N... 100644 100644 100644 hH iH path.txt\x00";
    let err = parse_porcelain_v2(buf).expect_err("double space should fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("empty field") || msg.contains("double space"),
        "got: {msg}"
    );
}

#[test]
fn truncated_field_count_rejected() {
    let buf = b"# branch.head main\x001 M. N...\x00";
    let err = parse_porcelain_v2(buf).expect_err("truncated record should fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("expected") || msg.contains("parts"),
        "got: {msg}"
    );
}

#[test]
fn type_2_missing_orig_path_rejected() {
    // Type-2 record without a following NUL-terminated orig path.
    let buf = b"# branch.head main\x002 R. N... 100644 100644 100644 hH iH R100 new.txt\x00";
    let err = parse_porcelain_v2(buf).expect_err("missing orig path should fail");
    let msg = format!("{err}");
    assert!(msg.contains("orig"), "got: {msg}");
}

#[test]
fn empty_buffer_yields_default() {
    // Defensive: callers may feed an empty slice (pre-first-poll). No panic.
    let s = parse_porcelain_v2(b"").expect("empty parses");
    assert_eq!(s.branch, None);
    assert!(s.files.is_empty());
}
