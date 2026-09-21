//! Keyboard navigation for the stash section: a cursor over the rows the
//! panel is currently painting, moved by the arrow keys and acted on by
//! `Enter` and `Cmd+Backspace`.
//!
//! # The cursor is panel state, not focus state
//!
//! `Cmd+Backspace` opens the Drop confirm dialog, which takes focus away from
//! the panel. A cursor expressed as "whichever row has focus" would be gone by
//! the time the user pressed Escape, and they would be back at the top of a
//! list they had arrowed halfway down. So the cursor is a field, it is keyed by
//! **sha** rather than by index (the stack is shared with every worktree and
//! with the user's terminal), and the only thing that clears it is the row it
//! names actually leaving the stack — see `StashPanel::adopt_list`.
//!
//! # Context-scoped bindings, not registry entries and not raw key handlers
//!
//! The plan called for the keybindings registry. The registry's inventory
//! builds every binding with `KeyBinding::new(chord, action, None)` — no
//! context — so a registry entry for bare `up` / `down` / `left` / `right` /
//! `enter` would shadow those keys for the whole application: every list, every
//! input, the terminal. That is not a defect in the registry (nothing else in
//! it wants a bare arrow key), it is the wrong home for these five.
//!
//! They are installed the way the two other context-owning surfaces in the
//! cockpit install theirs — [`register_session_history_key_bindings`] and the
//! terminal's shadow bindings — as keymap actions scoped to a `key_context`
//! the panel's root carries. Keymap actions rather than an `on_key_down` case
//! because macOS delivers Cmd-modified keys through `performKeyEquivalent:`,
//! which bypasses element key listeners entirely; `Cmd+Backspace` would never
//! arrive. The cost, disclosed: these five are not rebindable from the settings
//! pane, exactly like `Cmd+Enter` in the session-history modal.
//!
//! # Opening a diff hands focus to the diff
//!
//! `Enter` on a file row, and a plain click on one, both open a diff tab — and
//! the tab takes keyboard focus, which takes `StashPanel` off the dispatch
//! chain. The arrow keys are then dead until the panel is focused again (click
//! a stash row, which opens nothing, or Tab back). **This is not a defect in
//! the cursor**; it is what opening a tab means everywhere in the cockpit, and
//! it is the same thing an editor does when you open a file from a tree.
//!
//! It is recorded here because it looks exactly like a broken binding from the
//! outside, and it cost a live-verification pass to tell the two apart: `↑`
//! appeared to work from some rows and not others, and the difference was
//! only ever whether the previous interaction had opened a tab. If keeping the
//! cursor live across an open is ever wanted, the change belongs in the tab
//! system's focus policy, not here.
//!
//! # The context sits on the LIST, not on the section
//!
//! The section also owns a keyboard resize rail, a focusable sibling whose own
//! `on_key_down` takes Arrow / Shift+Arrow / Home / End to change the section's
//! height. Putting `StashPanel` on their common parent would leave a focused
//! rail with the context still on its dispatch chain, so one arrow keystroke
//! would match a cursor binding AND reach the rail's handler. The context and
//! the panel's focus handle therefore live on the scrollable list element; each
//! surface owns the arrow keys exactly while it is the one being driven.
//!
//! [`register_session_history_key_bindings`]:
//!     crate::shell::session_history::register_session_history_key_bindings

use gpui::{App, Context, KeyBinding, Window, point, px};
use std::path::PathBuf;

use crate::actions::{
    StashCursorActivate, StashCursorCollapse, StashCursorDown, StashCursorDrop,
    StashCursorExpand, StashCursorUp,
};
use crate::shell::source_control::tree::NodeKind;
use crate::shell::stash_panel::{StashFilesState, StashListState, StashPanel, tree_view};
use oximux_core::ViewMode;

/// Key context carried by the panel's root while it is focused, so the five
/// bindings below only participate in dispatch when the stash list is the
/// surface the user is driving.
pub const STASH_PANEL_KEY_CONTEXT: &str = "StashPanel";

/// Install the panel-scoped navigation bindings. Called once at boot from
/// `keybindings_settings::install`, alongside the other context-scoped sets.
pub fn register_stash_panel_key_bindings(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("up", StashCursorUp, Some(STASH_PANEL_KEY_CONTEXT)),
        KeyBinding::new("down", StashCursorDown, Some(STASH_PANEL_KEY_CONTEXT)),
        KeyBinding::new("right", StashCursorExpand, Some(STASH_PANEL_KEY_CONTEXT)),
        KeyBinding::new("left", StashCursorCollapse, Some(STASH_PANEL_KEY_CONTEXT)),
        KeyBinding::new("enter", StashCursorActivate, Some(STASH_PANEL_KEY_CONTEXT)),
        // Destructive, and it still goes through the confirm dialog — the
        // keyboard path fires the same event the trash glyph does, so there is
        // one confirm step and not two code paths to keep honest.
        KeyBinding::new(
            "secondary-backspace",
            StashCursorDrop,
            Some(STASH_PANEL_KEY_CONTEXT),
        ),
    ]);
}

/// Where the cursor is.
///
/// Identified by sha, never by the index the row was painted with. Folder rows
/// in tree mode are deliberately **not** cursor targets: `→` / `←` already mean
/// expand / collapse for the stash row, and giving a folder the same two keys
/// with a different subject is how a list stops being predictable. A file under
/// a collapsed folder is simply not in the visible-row list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StashCursor {
    Stash(String),
    File { sha: String, path: PathBuf },
}

impl StashCursor {
    /// The stash this row belongs to — the identity everything is keyed by.
    pub fn sha(&self) -> &str {
        match self {
            StashCursor::Stash(sha) => sha,
            StashCursor::File { sha, .. } => sha,
        }
    }
}

impl StashPanel {
    /// Current cursor, for the row renderers and for tests.
    pub fn cursor(&self) -> Option<&StashCursor> {
        self.cursor.as_ref()
    }

    /// Point the cursor at a row directly. Used by the click path, so arrowing
    /// continues from what the user last touched rather than from the top.
    pub fn set_cursor(&mut self, cursor: StashCursor, cx: &mut Context<Self>) {
        self.cursor = Some(cursor);
        cx.notify();
    }


    /// Every row the panel is currently painting, top to bottom.
    ///
    /// Derived on demand rather than cached: it is a function of the list, the
    /// expansion set, the file cache and (in tree mode) the collapsed folders,
    /// and a cache of four inputs is four chances to paint a cursor on a row
    /// that is not there. The lists involved are small — a stash stack is
    /// single digits and one stash's file list is tens.
    pub fn visible_rows(&self) -> Vec<StashCursor> {
        let StashListState::Ready(entries) = &self.state else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for entry in entries {
            rows.push(StashCursor::Stash(entry.sha.clone()));
            if !self.is_expanded(&entry.sha) {
                continue;
            }
            let Some(StashFilesState::Ready(files)) = self.files.get(&entry.sha) else {
                // Loading and Failed both paint a single note row, which is
                // not something there is anything to do on.
                continue;
            };
            for path in self.visible_file_paths(&entry.sha, files) {
                rows.push(StashCursor::File {
                    sha: entry.sha.clone(),
                    path,
                });
            }
        }
        rows
    }

    /// Move the cursor by one row, wrapping at neither end.
    ///
    /// An empty cursor takes the first row on `↓` and the last on `↑`, so the
    /// first keystroke after focusing the panel always lands somewhere.
    ///
    /// Public and `Window`-free, like every other method in this block: the
    /// action handlers below are two-line wrappers, so what is worth asserting
    /// is reachable from a `#[gpui::test]` that has no `&mut Window` to hand.
    /// Same split as `apply_list_result`.
    pub fn cursor_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        let rows = self.visible_rows();
        if rows.is_empty() {
            return;
        }
        let next = match self.cursor.as_ref().and_then(|c| rows.iter().position(|r| r == c)) {
            Some(i) => (i as isize + delta).clamp(0, rows.len() as isize - 1) as usize,
            // No cursor, or one pointing at a row that is no longer painted
            // (a stash collapsed under it). Enter the list from the end the
            // user is heading towards.
            None if delta > 0 => 0,
            None => rows.len() - 1,
        };
        self.cursor = Some(rows[next].clone());
        self.scroll_cursor_into_view();
        cx.notify();
    }

    /// Scroll the list so the cursor row is inside the viewport.
    ///
    /// **Without this the arrow keys are unusable past the first screenful.**
    /// The body is bounded (the user drags its height; 30 stashes paint about
    /// ten rows), so arrowing simply walked the cursor off the bottom and left
    /// the user pressing a key with nothing visibly happening. Found live with
    /// a 30-stash stack — six stashes, the size the earlier phases were
    /// verified at, never filled the viewport and so never showed it.
    ///
    /// Arithmetic rather than `ScrollHandle::scroll_to_item`, which only
    /// understands the scroll container's DIRECT children — here a single
    /// wrapper column, so it could scroll to the whole list and nothing
    /// finer (the same limitation `left_rail/locate_anchor.rs` documents).
    /// Arithmetic is exact here for a reason worth stating: every row this
    /// body paints — stash, file, tree folder, and the loading/failed/empty
    /// note — is exactly `density.h_row` tall. If that ever stops being true,
    /// this goes wrong silently, so [`painted_index`] carries the same note.
    ///
    /// [`painted_index`]: Self::painted_index
    fn scroll_cursor_into_view(&self) {
        let Some(cursor) = self.cursor.as_ref() else {
            return;
        };
        let Some(ix) = self.painted_index(cursor) else {
            return;
        };
        let view_h = f32::from(self.scroll_handle.bounds().size.height);
        // Never laid out yet: there is no viewport to reveal anything in.
        if view_h <= 0.0 {
            return;
        }
        let row_h = self.density.h_row;
        let offset = self.scroll_handle.offset();
        let current = f32::from(offset.y);
        let top = ix as f32 * row_h;
        // The offset runs from -max (scrolled to the bottom) to 0.
        let on_screen_top = top + current;
        let target = if on_screen_top < 0.0 {
            -top
        } else if on_screen_top + row_h > view_h {
            view_h - (top + row_h)
        } else {
            return;
        };
        let max = f32::from(self.scroll_handle.max_offset().y);
        self.scroll_handle
            .set_offset(point(offset.x, px(target.clamp(-max, 0.0))));
    }

    /// Position of `cursor` among **every row the body is currently painting**,
    /// including the tree's folder rows and the one-line loading/failed/empty
    /// notes — rows the cursor itself never lands on, but which still take up
    /// vertical space and therefore still shift everything below them.
    ///
    /// This walks the body in the same order `file_row::render_files` paints
    /// it. The two must agree; a drift here scrolls to the wrong row rather
    /// than failing, which is why the walk mirrors the renderer's structure
    /// arm for arm instead of being derived from `visible_rows`.
    pub fn painted_index(&self, cursor: &StashCursor) -> Option<usize> {
        let StashListState::Ready(entries) = &self.state else {
            return None;
        };
        let mut ix = 0usize;
        for entry in entries {
            if matches!(cursor, StashCursor::Stash(sha) if *sha == entry.sha) {
                return Some(ix);
            }
            ix += 1;
            if !self.is_expanded(&entry.sha) {
                continue;
            }
            match self.files.get(&entry.sha) {
                Some(StashFilesState::Ready(files)) if !files.is_empty() => match self.view_mode() {
                    ViewMode::Flat => {
                        for f in files {
                            if matches!(
                                cursor,
                                StashCursor::File { sha, path }
                                    if *sha == entry.sha && *path == f.path
                            ) {
                                return Some(ix);
                            }
                            ix += 1;
                        }
                    }
                    ViewMode::Tree => {
                        for row in tree_view::rows(files, &self.collapsed_for(&entry.sha)) {
                            if row.kind == NodeKind::File
                                && matches!(
                                    cursor,
                                    StashCursor::File { sha, path }
                                        if *sha == entry.sha && *path == row.path
                                )
                            {
                                return Some(ix);
                            }
                            ix += 1;
                        }
                    }
                },
                // Loading, Failed, and a Ready-but-empty list each paint
                // exactly one note row. So does a sha with no cache entry at
                // all, which is what an expand-in-flight looks like.
                _ => ix += 1,
            }
        }
        None
    }

    /// `→` — expand the stash under the cursor. A no-op on a file row and on
    /// an already-open stash, rather than falling through to "move down": a
    /// key that sometimes navigates and sometimes does not is worse than one
    /// that reliably does one thing.
    pub fn cursor_expand(&mut self, cx: &mut Context<Self>) {
        let Some(StashCursor::Stash(sha)) = self.cursor.clone() else {
            return;
        };
        if !self.is_expanded(&sha) {
            self.toggle_expanded(sha, cx);
        }
    }

    /// `←` — collapse an open stash, or step from a file row up to the stash
    /// that owns it. The second half is the list convention that makes a deep
    /// expansion escapable without arrowing back through every file.
    pub fn cursor_collapse(&mut self, cx: &mut Context<Self>) {
        match self.cursor.clone() {
            // Only when it is open: `←` on an already-collapsed row is a
            // no-op, not a toggle, so holding the key cannot flap a row open
            // and shut.
            Some(StashCursor::Stash(sha)) if self.is_expanded(&sha) => {
                self.toggle_expanded(sha, cx);
            }
            Some(StashCursor::Stash(_)) => {}
            Some(StashCursor::File { sha, .. }) => {
                self.cursor = Some(StashCursor::Stash(sha));
                // Climbing out of a deep expansion can land far above the
                // viewport; reveal it like any other cursor move.
                self.scroll_cursor_into_view();
                cx.notify();
            }
            None => {}
        }
    }

    /// `Enter` — open the diff on a file row, toggle the expansion on a stash
    /// row. Both are the row's own primary click, so the keyboard and the
    /// mouse cannot drift apart.
    pub fn cursor_activate(&mut self, cx: &mut Context<Self>) {
        match self.cursor.clone() {
            Some(StashCursor::Stash(sha)) => self.toggle_expanded(sha, cx),
            Some(StashCursor::File { sha, path }) => self.request_file_diff(&sha, &path, cx),
            None => {}
        }
    }

    /// `Cmd+Backspace` — Drop the stash under the cursor, through the same
    /// confirm dialog the trash glyph opens.
    ///
    /// On a FILE row it drops the stash that owns it rather than doing
    /// nothing. That is deliberate and it is the riskier of the two readings,
    /// so it is worth being explicit: there is no per-file delete in a stash,
    /// the confirm dialog names the stash it is about to remove, and the
    /// alternative — a destructive key that silently does nothing on half the
    /// rows — teaches the user the binding is unreliable.
    pub fn cursor_drop(&mut self, cx: &mut Context<Self>) {
        let Some(cursor) = self.cursor.clone() else {
            return;
        };
        let StashListState::Ready(entries) = &self.state else {
            return;
        };
        let Some(entry) = entries.iter().find(|e| e.sha == cursor.sha()) else {
            return;
        };
        cx.emit(crate::shell::stash_panel::DropStashRequested {
            stash_ref: entry.stash_ref.clone(),
            sha: entry.sha.clone(),
            message: entry.message.clone(),
            relative: entry.relative.clone(),
            branch: entry.branch.clone(),
        });
    }

    // ── Action handlers ────────────────────────────────────────────────
    //
    // Two lines each, on purpose. Everything worth asserting lives above,
    // where a test can reach it without a `&mut Window` — the GPUI test
    // scheduler gives a `#[gpui::test]` no way to conjure one.

    pub(crate) fn on_cursor_up(
        &mut self,
        _: &StashCursorUp,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cursor_move(-1, cx);
    }

    pub(crate) fn on_cursor_down(
        &mut self,
        _: &StashCursorDown,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cursor_move(1, cx);
    }

    pub(crate) fn on_cursor_expand(
        &mut self,
        _: &StashCursorExpand,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cursor_expand(cx);
    }

    pub(crate) fn on_cursor_collapse(
        &mut self,
        _: &StashCursorCollapse,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cursor_collapse(cx);
    }

    pub(crate) fn on_cursor_activate(
        &mut self,
        _: &StashCursorActivate,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cursor_activate(cx);
    }

    pub(crate) fn on_cursor_drop(
        &mut self,
        _: &StashCursorDrop,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cursor_drop(cx);
    }
}
