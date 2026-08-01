//! Keep every data path resolving through `app_paths`.
//!
//! `app_paths` exists because ten modules each spelled
//! `dirs::data_dir().map(|d| d.join("dev.nhtera.oximux"))` themselves. The
//! migration that introduced it converted most of them and missed seven, and
//! nothing noticed for a simple reason: on macOS `dirs::data_dir()` and
//! `dirs::data_local_dir()` are the same directory, so the two spellings are
//! indistinguishable there. On Windows they are not — `data_dir` is the
//! *roaming* profile — and the leftovers quietly wrote to a second directory.
//!
//! What that cost: the relay daemon writes scrollback checkpoints under the
//! local runtime dir, while `default_checkpoints_dir()` read the roaming one,
//! so crash restore looked in a directory that did not exist and reported
//! "nothing to restore" — indistinguishable from an empty checkpoint store.
//! The screen-control grant store landed in the roaming profile too, which on
//! a domain-joined machine follows the user to other machines; a record of
//! which agent may drive this desktop is the last thing that should roam.
//!
//! A unit test cannot catch this. The obvious one — assert the two `dirs`
//! functions agree — only ever ran on macOS, where it is vacuously true, and
//! asserting they *differ* on Windows tests `dirs`, not us. The property that
//! actually matters is a source property: nothing outside `app_paths` names
//! these functions. So it is linted, not tested.
//!
//! `home_dir` is deliberately not covered. Reading `~/.claude` or `~/.codex`
//! is reaching into *another* tool's files, which is a different thing from
//! deciding where OxiMux keeps its own.

use std::error::Error;
use std::path::{Path, PathBuf};

/// The `dirs` roots that pick a location for OxiMux's own files. Every one of
/// these has an `app_paths` wrapper that encodes the platform reasoning once.
const RESERVED: &[(&str, &str)] = &[
    ("dirs::data_dir()", "app_paths::data_dir()"),
    ("dirs::data_local_dir()", "app_paths::data_dir()"),
    ("dirs::cache_dir()", "app_paths::cache_dir()"),
];

/// The one module allowed to call them: the place the decision is made.
const OWNER: &str = "apps/desktop/src/app_paths.rs";

/// Repo-relative paths of files that may still name them, with a reason.
/// Empty, and meant to stay that way — a new entry needs an argument for why
/// this particular file gets to decide a location for itself.
const ALLOW: &[(&str, &str)] = &[];

pub fn run(sources: &[PathBuf], root: &Path) -> Result<(), Box<dyn Error>> {
    let mut hits: Vec<String> = Vec::new();

    for file in sources {
        let rel = file
            .strip_prefix(root)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");
        if rel == OWNER || ALLOW.iter().any(|(p, _)| *p == rel) {
            continue;
        }
        let text = std::fs::read_to_string(file)?;
        for (line_no, line) in text.lines().enumerate() {
            // Comments and doc comments name these functions constantly while
            // explaining the very rule this lint enforces; only code counts.
            let code = line.trim_start();
            if code.starts_with("//") || code.starts_with("*") {
                continue;
            }
            for (needle, replacement) in RESERVED {
                if line.contains(needle) {
                    hits.push(format!(
                        "  {rel}:{}: {needle} — use {replacement}",
                        line_no + 1
                    ));
                }
            }
        }
    }

    if hits.is_empty() {
        return Ok(());
    }
    hits.sort();
    Err(format!(
        "{} data-path call site(s) bypass app_paths:\n{}\n\n\
         These decide where OxiMux keeps its files, and the decision belongs in\n\
         {OWNER} so it is made once per platform rather than once per module.\n\
         On macOS data_dir and data_local_dir are the same directory, so a\n\
         bypass here is invisible until it reaches Windows, where data_dir is\n\
         the roaming profile and the app's state ends up split in two.",
        hits.len(),
        hits.join("\n")
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write `files` under a fresh temp root and run the lint over them.
    fn lint(files: &[(&str, &str)]) -> Result<(), Box<dyn Error>> {
        let root = std::env::temp_dir().join(format!(
            "oximux-data-dir-lint-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut paths = Vec::new();
        for (rel, body) in files {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent"))?;
            std::fs::write(&path, body)?;
            paths.push(path);
        }
        let out = run(&paths, &root);
        let _ = std::fs::remove_dir_all(&root);
        out
    }

    #[test]
    fn a_bypass_is_rejected_and_named() {
        let err = lint(&[(
            "apps/desktop/src/thing.rs",
            "fn p() { dirs::data_dir().join(\"x\") }\n",
        )])
        .expect_err("a direct dirs::data_dir() call must fail the lint");
        let msg = err.to_string();
        assert!(msg.contains("thing.rs:1"), "should name the line: {msg}");
        assert!(
            msg.contains("app_paths::data_dir()"),
            "should name the replacement: {msg}"
        );
    }

    #[test]
    fn the_roaming_spelling_and_the_local_one_are_both_caught() {
        // Both are bypasses. `data_local_dir` happens to resolve correctly
        // today, but it re-decides the platform question in a second place,
        // which is how the two spellings drifted apart to begin with.
        let err = lint(&[
            ("a.rs", "let x = dirs::data_dir();\n"),
            ("b.rs", "let y = dirs::data_local_dir();\n"),
            ("c.rs", "let z = dirs::cache_dir();\n"),
        ])
        .expect_err("all three roots must fail");
        assert!(err.to_string().contains("3 data-path call site(s)"));
    }

    #[test]
    fn the_owning_module_may_call_them() {
        lint(&[(
            "apps/desktop/src/app_paths.rs",
            "pub fn data_dir() { dirs::data_local_dir() }\n",
        )])
        .expect("app_paths is where the decision is made");
    }

    #[test]
    fn prose_about_the_rule_is_not_a_violation() {
        // These modules explain the roaming-vs-local reasoning in comments;
        // matching on raw text would make documenting the rule break it.
        lint(&[(
            "apps/desktop/src/notes.rs",
            "// `dirs::data_dir()` is the roaming profile on Windows.\n\
             /// Prefer app_paths over dirs::data_dir() here.\n\
             fn ok() { app_paths::data_dir() }\n",
        )])
        .expect("comments naming the function are fine");
    }

    #[test]
    fn home_dir_is_left_alone() {
        // Reading another tool's config out of the home directory is not a
        // decision about where OxiMux keeps its own files.
        lint(&[(
            "apps/desktop/src/hooks.rs",
            "fn s() { dirs::home_dir().map(|h| h.join(\".claude\")) }\n",
        )])
        .expect("home_dir is not covered by this lint");
    }
}
