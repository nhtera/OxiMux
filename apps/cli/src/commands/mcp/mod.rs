//! `oximux mcp`: the simulator attached to one worktree, served to an outside
//! MCP client (an agent app that is not OxiMux's own) over stdio.
//!
//! A translator and nothing more: every tool runs the CLI's own `sim` verb
//! ([`super::sim::run`]) through the same version gates, so its behaviour and
//! its text are the CLI's. What differs is who asks: the connection presents
//! the MCP credential, and the host keeps that principal's consent apart from
//! agents' (the user is asked about "an outside MCP client", and nothing an
//! agent was allowed carries over).
//!
//! One server serves one worktree, fixed at start: `--worktree`, or the
//! current directory when it is a worktree OxiMux knows. Stdout carries only
//! JSON-RPC; anything else goes to stderr.

mod server;
mod tools;

use std::path::{Path, PathBuf};

use crate::cli::exit;
use crate::client::Client;

/// What every tool call needs.
pub(super) struct Ctx {
    /// The host's runtime directory (`--dir`), as for any verb.
    dir: Option<PathBuf>,
    worktree: PathBuf,
}

pub fn run(dir: Option<PathBuf>, worktree: Option<PathBuf>) -> u8 {
    let explicit = worktree.is_some();
    let worktree = match chosen_worktree(worktree, std::env::current_dir().ok(), dirs::home_dir()) {
        Ok(path) => path,
        Err(why) => {
            eprintln!("oximux mcp: {why}");
            return exit::USAGE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("oximux mcp: cannot start: {e}");
            return exit::ERROR;
        }
    };
    let ctx = Ctx { dir, worktree };
    runtime.block_on(async move {
        if !explicit && let Err(why) = known_worktree(&ctx).await {
            eprintln!("oximux mcp: {why}");
            return exit::USAGE;
        }
        server::serve(ctx).await;
        exit::OK
    })
}

/// `--worktree` (made absolute), else the current directory — never `/` or
/// the home directory, where "the worktree" would mean whatever the client
/// happened to start in.
fn chosen_worktree(flag: Option<PathBuf>, cwd: Option<PathBuf>, home: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(path) = flag {
        return Ok(match (&cwd, path.is_absolute()) {
            (Some(cwd), false) => cwd.join(path),
            _ => path,
        });
    }
    let cwd = cwd.ok_or("cannot read the current directory; pass --worktree PATH")?;
    // Compared as the filesystem resolves them: a cwd reads back resolved
    // (`/private/var/…`) where `$HOME` may name a symlink.
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    // A root (`/`, `C:\`) has no parent.
    if real(&cwd).parent().is_none() || home.as_deref().is_some_and(|home| real(home) == real(&cwd)) {
        return Err(format!(
            "started in {}, which is not a worktree; pass --worktree PATH (your repository) in the client's config",
            cwd.display()
        ));
    }
    Ok(cwd)
}

/// Started without `--worktree`: refuse a directory OxiMux does not know. A
/// host that is not running yet is no reason to refuse — each tool says so.
async fn known_worktree(ctx: &Ctx) -> Result<(), String> {
    let Ok(client) = Client::connect_mcp(ctx.dir.clone(), 5).await else { return Ok(()) };
    match super::sim::run(&client, Some(ctx.worktree.clone()), crate::cli::SimCommand::Status).await {
        Err(f) if f.code == "no-device" => Err(format!(
            "{} is not in a worktree OxiMux knows; pass --worktree PATH, or open the project in OxiMux",
            ctx.worktree.display()
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_worktree_is_never_root_or_home() {
        let me = std::env::temp_dir().join("oximux-mcp-home");
        let root = me.ancestors().last().unwrap().to_path_buf();
        let home = Some(me.clone());
        assert!(chosen_worktree(None, Some(root.clone()), home.clone()).is_err());
        assert!(chosen_worktree(None, Some(me.clone()), home.clone()).is_err());
        assert_eq!(chosen_worktree(None, Some(me.join("app")), home.clone()), Ok(me.join("app")));
        assert_eq!(chosen_worktree(Some("app".into()), Some(me.clone()), home.clone()), Ok(me.join("app")), "relative to where it starts");
        assert_eq!(chosen_worktree(Some(me.join("r")), Some(root), home), Ok(me.join("r")), "--worktree may be anywhere");
    }
}
