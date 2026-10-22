//! The palette's Insert Path mode: pick a file or directory under a
//! terminal's directory, and hand it back for typing at that terminal's
//! cursor.
//!
//! It keeps an index of its own, keyed by root: Quick Open's per-project index
//! is built once and never re-rooted, and a terminal's directory is usually
//! not the project root. Directories are listed too (each file's parents,
//! deduplicated, with a trailing `/`). A path holding a control byte is left
//! out — typed at a prompt, it would act, not appear.

use std::collections::BTreeSet;
use std::path::PathBuf;

use gpui::{Context, Task, Window};
use tokio::sync::oneshot;

use super::entry::PaletteMode;
use super::file_index;
use super::match_engine::filter_and_rank;
use super::{PaletteEvent, PaletteModal};
use crate::shell::terminal::shell_quote::has_control;

/// Rows shown at once, like Quick Open.
const MAX_ROWS: usize = 50;

#[derive(Default)]
pub(super) struct InsertPathIndex {
    root: Option<PathBuf>,
    /// Root-relative entries; directories end in `/`.
    entries: Vec<String>,
    scanning: bool,
    error: Option<String>,
    _task: Option<Task<()>>,
}

/// Files plus every directory above them, without any control-byte path.
/// Separators are `/` everywhere, so folders are found on Windows too.
fn with_directories(files: Vec<String>) -> Vec<String> {
    let files: Vec<String> =
        if cfg!(windows) { files.into_iter().map(|f| f.replace('\\', "/")).collect() } else { files };
    let mut dirs = BTreeSet::new();
    for file in &files {
        let mut end = 0;
        while let Some(i) = file[end..].find('/') {
            end += i + 1;
            dirs.insert(file[..end].to_owned());
        }
    }
    dirs.into_iter().chain(files).filter(|p| !has_control(p)).collect()
}

impl PaletteModal {
    /// Open in Insert Path mode over `root`, re-indexing it every time: files
    /// come and go between picks, and a failed scan must not stick. The last
    /// list for the same root stays up while the new one is built.
    pub fn open_insert_path(&mut self, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        {
            let keep = if self.insert_index.root.as_ref() == Some(&root) {
                std::mem::take(&mut self.insert_index.entries)
            } else {
                Vec::new()
            };
            self.insert_index = InsertPathIndex {
                root: Some(root.clone()),
                entries: keep,
                scanning: true,
                ..InsertPathIndex::default()
            };
            let (tx, rx) = oneshot::channel();
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    handle.spawn(async move {
                        let _ = tx.send(file_index::scan_files(root).await);
                    });
                    self.insert_index._task = Some(cx.spawn(async move |this, cx| {
                        let res = rx.await;
                        let _ = this.update(cx, |p, cx| {
                            let index = &mut p.insert_index;
                            index.scanning = false;
                            match res {
                                Ok(Ok(files)) => index.entries = with_directories(files),
                                Ok(Err(e)) => index.error = Some(e.hint()),
                                Err(_) => index.error = Some("file scan ended unexpectedly".into()),
                            }
                            cx.notify();
                        });
                    }));
                }
                Err(_) => {
                    self.insert_index.scanning = false;
                    self.insert_index.error = Some("no async runtime available for file scan".into());
                }
            }
        }
        self.open(PaletteMode::InsertPath, window, cx);
    }

    /// Ranked rows for the current query.
    pub(super) fn insert_path_matches(&self) -> Vec<String> {
        let entries = &self.insert_index.entries;
        let names: Vec<&str> = entries.iter().map(String::as_str).collect();
        filter_and_rank(&self.query, &names)
            .into_iter()
            .take(MAX_ROWS)
            .map(|i| entries[i].clone())
            .collect()
    }

    /// The status line shown instead of rows when there are none.
    pub(super) fn insert_path_hint(&self) -> &str {
        let index = &self.insert_index;
        match (&index.error, index.scanning) {
            (Some(err), _) => err,
            (None, true) => "Indexing…",
            (None, false) => "No matching paths",
        }
    }

    /// Hand the picked path to whoever opened the picker, then close.
    pub(super) fn activate_insert_path(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(rel) = self.insert_path_matches().get(idx).cloned() else {
            return;
        };
        let Some(root) = self.insert_index.root.clone() else {
            return;
        };
        // `join` keeps a directory's trailing `/`, which marks it as one.
        cx.emit(PaletteEvent::InsertPath(root.join(rel)));
        self.close(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directories_are_listed_once_and_hostile_names_never() {
        let entries = with_directories(vec![
            "src/main.rs".into(),
            "src/a/b.rs".into(),
            "README.md".into(),
            "bad\u{3}name".into(),
            "evil\r/x".into(),
        ]);
        // Directories first (sorted, once each), then the files in scan
        // order; anything holding a control byte is gone, directory or file.
        assert_eq!(entries, ["src/", "src/a/", "src/main.rs", "src/a/b.rs", "README.md"]);
    }
}
