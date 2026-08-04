//! `oximux git` — status, diffs, staging, and commits for a session's
//! repository. Paths are echoes of what `git status` listed; the host
//! re-contains every one, so nothing here validates filesystem reality.

use oximux_remote_proto::messages::{
    DiffLineKindWire, FileDiffWire, GitFileWire, GitStatusWire, IndexStatusWire,
    WorktreeStatusWire,
};
use oximux_remote_proto::proto::{Request, Response};
use serde_json::{Value, json};

use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::output::Failure;

/// Porcelain-style two-letter code for a row.
fn status_code(file: &GitFileWire) -> String {
    let x = match file.index {
        IndexStatusWire::Unmodified => ' ',
        IndexStatusWire::Modified => 'M',
        IndexStatusWire::Added => 'A',
        IndexStatusWire::Deleted => 'D',
        IndexStatusWire::Renamed => 'R',
        IndexStatusWire::Copied => 'C',
        IndexStatusWire::Untracked => '?',
        IndexStatusWire::Ignored => '!',
        IndexStatusWire::Unmerged => 'U',
    };
    let y = match file.worktree {
        WorktreeStatusWire::Unmodified => ' ',
        WorktreeStatusWire::Modified => 'M',
        WorktreeStatusWire::Deleted => 'D',
        WorktreeStatusWire::Renamed => 'R',
        WorktreeStatusWire::Untracked => '?',
        WorktreeStatusWire::Ignored => '!',
        WorktreeStatusWire::Unmerged => 'U',
    };
    format!("{x}{y}")
}

fn status_json(status: &GitStatusWire) -> Value {
    json!({
        "branch": status.branch,
        "upstream": status.upstream,
        "ahead": status.ahead,
        "behind": status.behind,
        "files": status.files.iter().map(|f| json!({
            "path": f.path,
            "code": status_code(f),
            "unstaged_lines": f.unstaged_lines,
            "staged_lines": f.staged_lines,
        })).collect::<Vec<_>>(),
    })
}

pub async fn status(client: &Client, session: &str) -> Result<(Value, String), Failure> {
    let status = match client.call(Request::GitStatus { session_id: session.into() }).await? {
        Response::GitStatus(s) => s,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("GitStatus", &other)),
    };
    let mut lines = Vec::new();
    let branch = status.branch.as_deref().unwrap_or("(detached HEAD)");
    let mut head = format!("on {branch}");
    if let Some(upstream) = &status.upstream {
        head.push_str(&format!("  ({upstream} +{} -{})", status.ahead, status.behind));
    }
    lines.push(head);
    if status.files.is_empty() {
        lines.push("clean".into());
    } else {
        for file in &status.files {
            lines.push(format!("{}  {}", status_code(file), file.path));
        }
    }
    Ok((status_json(&status), lines.join("\n")))
}

/// Render one file's hunks as unified-diff-style text.
fn render_diff(file: &FileDiffWire) -> String {
    let mut out = format!("--- {}\n", file.path);
    for hunk in &file.hunks {
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@ {}\n",
            hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines, hunk.header_suffix
        ));
        for line in &hunk.lines {
            let marker = match line.kind {
                DiffLineKindWire::Context => ' ',
                DiffLineKindWire::Added => '+',
                DiffLineKindWire::Removed => '-',
                DiffLineKindWire::NoNewlineHint => '\\',
            };
            out.push(marker);
            out.push_str(&line.content);
            out.push('\n');
        }
    }
    out
}

pub async fn diff(
    client: &Client,
    session: &str,
    path: &str,
    staged: bool,
    untracked: bool,
) -> Result<(Value, String), Failure> {
    let files = match client
        .call(Request::GitDiff { session_id: session.into(), path: path.into(), staged, untracked })
        .await?
    {
        Response::GitDiff(files) => files,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("GitDiff", &other)),
    };
    let human = if files.is_empty() {
        "no changes".to_string()
    } else {
        files.iter().map(render_diff).collect::<Vec<_>>().join("\n")
    };
    let data = serde_json::to_value(&files).unwrap_or(Value::Null);
    Ok((data, human))
}

pub async fn stage(
    client: &Client,
    session: &str,
    paths: Vec<String>,
) -> Result<(Value, String), Failure> {
    let n = paths.len();
    match client.call(Request::GitStage { session_id: session.into(), paths }).await? {
        Response::Ack => Ok((json!({ "staged": n }), format!("staged {n} path(s)"))),
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("GitStage", &other)),
    }
}

pub async fn unstage(
    client: &Client,
    session: &str,
    paths: Vec<String>,
) -> Result<(Value, String), Failure> {
    let n = paths.len();
    match client.call(Request::GitUnstage { session_id: session.into(), paths }).await? {
        Response::Ack => Ok((json!({ "unstaged": n }), format!("unstaged {n} path(s)"))),
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("GitUnstage", &other)),
    }
}

pub async fn commit(
    client: &Client,
    session: &str,
    message: &str,
) -> Result<(Value, String), Failure> {
    match client
        .call(Request::GitCommit { session_id: session.into(), message: message.into() })
        .await?
    {
        Response::GitCommitted { sha } => {
            Ok((json!({ "sha": sha }), format!("committed {sha}")))
        }
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("GitCommit", &other)),
    }
}
