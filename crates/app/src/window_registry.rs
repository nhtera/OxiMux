//! Tracks every open workspace window so the app-level quit / window-closed
//! observers (registered once in `main.rs`) can reach each window's
//! [`WorkspaceRoot`] without every `open_window` closure registering its own
//! per-window observer.
//!
//! Why STRONG entity handles: the close/quit observers fire *after* GPUI has
//! already torn the window down, so the window can no longer hand back its
//! root view. The registry is therefore what keeps each `WorkspaceRoot` (and
//! transitively its panes + PTYs) alive long enough to capture session state
//! on the way out. Dropping a registry entry is the act that finally releases
//! that window's `TerminalView`s — which is how a non-last window close
//! tears down only its own PTYs and leaves the other windows untouched.
//!
//! Stored as a GPUI [`Global`]. A single-window run keeps exactly one entry
//! whose `persist_id` is `"main"` (the legacy single-window persistence key);
//! additional windows get minted ids (`"w1"`, `"w2"`, …) so per-window layout
//! / buffer / relay-id rows never collide.

use std::sync::{Arc, Mutex};

use gpui::{App, Entity, Global, SharedString, WindowId};

use crate::shell::pane_group::TabColor;
use crate::workspace_root::WorkspaceRoot;

// ---------------------------------------------------------------------------
// Pending tear-off mailbox
// ---------------------------------------------------------------------------

/// Per-leaf relay PTY identifier + display metadata for a torn-off tab.
/// Captured by the source window handler; consumed by the destination
/// window on first build. The `external_id` is the relay daemon's PTY id
/// that survives the source-window detach.
#[derive(Clone, Debug)]
pub struct PendingLeaf {
    pub external_id: String,
}

/// One pending tab tear-off: a destination window id, one or more relay
/// PTY leaves, and the tab's display state. The window-opener parameters
/// (repo / app_state / project) are NOT stored here — the source handler
/// passes them directly to `open_workspace_window_with`; this struct only
/// carries what the destination needs to reconstruct the tab.
///
/// The source handler: detaches every leaf PTY, takes the tab from its
/// group, mints `dest_window_id` via `next_persist_id`, pushes this struct,
/// then uses `cx.spawn_in` to open the destination window. The new window's
/// build closure calls `consume_pending_tearoff` to retrieve and mount the
/// tab from the relay PTY.
#[derive(Clone)]
pub struct PendingTearOff {
    /// Destination persist id — matches the id passed to
    /// `open_workspace_window_with`.
    pub dest_window_id: String,
    /// Relay PTY leaves in DFS order. v1 scope: always a single leaf
    /// (multi-leaf split trees are ineligible for tear-off and the menu
    /// item is disabled for them — see `TabContextMenu::open`).
    pub leaves: Vec<PendingLeaf>,
    /// Display label (e.g. "Terminal 3"). Used as the tab chip label in
    /// the destination window.
    pub label: SharedString,
    /// Optional user-assigned color tag.
    pub color: Option<TabColor>,
    /// Optional user-assigned custom title.
    pub custom_title: Option<SharedString>,
}

/// Process-global pending tear-off mailbox. Lazily initialised on first push.
/// `Arc<Mutex<_>>` so both the GPUI-thread writer and the GPUI-thread reader
/// share a single allocation without needing to thread through a context.
static PENDING_TEAROFFS: std::sync::OnceLock<Arc<Mutex<Vec<PendingTearOff>>>> =
    std::sync::OnceLock::new();

fn pending_store() -> Arc<Mutex<Vec<PendingTearOff>>> {
    PENDING_TEAROFFS
        .get_or_init(|| Arc::new(Mutex::new(Vec::new())))
        .clone()
}

/// Push a pending tear-off entry. Called by the source-window handler just
/// before `open_workspace_window_with` opens the destination window.
pub fn push_pending_tearoff(entry: PendingTearOff) {
    if let Ok(mut store) = pending_store().lock() {
        store.push(entry);
    } else {
        tracing::error!("pending tearoff store mutex poisoned; tear-off dropped");
    }
}

/// Remove and return the pending tear-off whose `dest_window_id` matches
/// `window_id`. Returns `None` when no entry is waiting (fresh window,
/// restore path, or prior consume already cleared the entry).
pub fn consume_pending_tearoff(window_id: &str) -> Option<PendingTearOff> {
    let store = pending_store();
    let mut guard = store.lock().ok()?;
    let pos = guard.iter().position(|e| e.dest_window_id == window_id)?;
    Some(guard.remove(pos))
}

/// Persistence id assigned to the first window. Matches the legacy
/// single-window key and the migration default, so existing single-window
/// state restores unchanged.
pub const PRIMARY_WINDOW_ID: &str = "main";

/// One tracked window: its GPUI id, the persistence-scoping id, and a STRONG
/// handle to its `WorkspaceRoot` (see module docs for why strong).
pub struct RegisteredWindow {
    pub window_id: WindowId,
    pub persist_id: String,
    pub workspace: Entity<WorkspaceRoot>,
}

/// App-global list of open workspace windows.
#[derive(Default)]
pub struct WindowRegistry {
    windows: Vec<RegisteredWindow>,
    /// Monotonic counter for minting non-primary persist ids. `0` means "no
    /// window opened yet" → the next mint is the primary `"main"`.
    minted: u64,
}

impl Global for WindowRegistry {}

impl WindowRegistry {
    /// Mint the persistence id for the next window. The first ever window is
    /// [`PRIMARY_WINDOW_ID`]; later windows get `"w{n}"`. The counter only
    /// grows, so ids stay unique across opens and closes. Pure — split out so
    /// the backward-compat "first window is `main`" contract is unit-testable
    /// without a GPUI context.
    fn mint_persist_id(&mut self) -> String {
        if self.minted == 0 {
            self.minted = 1;
            PRIMARY_WINDOW_ID.to_string()
        } else {
            let n = self.minted;
            self.minted += 1;
            format!("w{n}")
        }
    }

    /// Advance the mint counter past an id restored from the manifest, so a
    /// later `mint_persist_id` (a fresh Cmd+N window) never re-issues an id
    /// already in use — which would alias another window's persisted rows.
    /// `"main"` reserves slot 0; `"w{n}"` reserves through `n`.
    fn note_restored(&mut self, id: &str) {
        if id == PRIMARY_WINDOW_ID {
            self.minted = self.minted.max(1);
        } else if let Some(n) = id.strip_prefix('w').and_then(|s| s.parse::<u64>().ok()) {
            self.minted = self.minted.max(n + 1);
        }
    }
}

fn ensure_global(cx: &mut App) {
    if !cx.has_global::<WindowRegistry>() {
        cx.set_global(WindowRegistry::default());
    }
}

/// Mint the persistence id for the next window to open. The first ever window
/// is [`PRIMARY_WINDOW_ID`]; later windows get `"w{n}"`. The counter only
/// grows, so ids stay unique even as windows open and close.
pub fn next_persist_id(cx: &mut App) -> String {
    ensure_global(cx);
    cx.global_mut::<WindowRegistry>().mint_persist_id()
}

/// Record a freshly opened window. Called from inside the `open_window` build
/// closure once the `WorkspaceRoot` entity and the GPUI window id exist.
pub fn register(
    cx: &mut App,
    window_id: WindowId,
    persist_id: String,
    workspace: Entity<WorkspaceRoot>,
) {
    ensure_global(cx);
    cx.global_mut::<WindowRegistry>().windows.push(RegisteredWindow {
        window_id,
        persist_id,
        workspace,
    });
}

/// Strong `(persist_id, workspace)` clones for every tracked window. Cloned
/// out so the caller can freely use `cx` afterwards (e.g. to `read` each
/// entity) without holding a borrow on the global.
pub fn all_windows(cx: &App) -> Vec<(String, Entity<WorkspaceRoot>)> {
    cx.try_global::<WindowRegistry>()
        .map(|reg| {
            reg.windows
                .iter()
                .map(|w| (w.persist_id.clone(), w.workspace.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Remove the entry for `window_id`, returning it so the caller can capture
/// state before the strong `WorkspaceRoot` handle drops. `None` when the id
/// isn't tracked (e.g. a non-workspace spike window).
pub fn remove(cx: &mut App, window_id: WindowId) -> Option<RegisteredWindow> {
    if !cx.has_global::<WindowRegistry>() {
        return None;
    }
    let reg = cx.global_mut::<WindowRegistry>();
    reg.windows
        .iter()
        .position(|w| w.window_id == window_id)
        .map(|pos| reg.windows.remove(pos))
}

/// How many workspace windows remain tracked.
pub fn remaining(cx: &App) -> usize {
    cx.try_global::<WindowRegistry>()
        .map(|reg| reg.windows.len())
        .unwrap_or(0)
}

/// Reserve an id restored from the manifest so a later `next_persist_id`
/// (Cmd+N) can't re-issue it. Call once per restored window id at boot,
/// BEFORE opening the windows.
pub fn note_restored_id(cx: &mut App, id: &str) {
    ensure_global(cx);
    cx.global_mut::<WindowRegistry>().note_restored(id);
}

/// Capture every tracked window's session state (layout + scrollback +
/// relay ids) and write the open-windows manifest, so the next launch
/// reopens the same windows on the same projects. Called from the app-quit
/// and last-window-close hooks. No-op when no windows are tracked (so a
/// stray late call can't wipe a manifest written moments earlier).
pub fn capture_session(cx: &mut App) {
    let windows = all_windows(cx);
    if windows.is_empty() {
        return;
    }
    let mut manifest = crate::persisted_terminals::WindowsManifest::default();
    let mut settings_repo: Option<oximux_storage::SettingsRepo> = None;
    for (persist_id, workspace) in &windows {
        let root = workspace.read(cx);
        root.capture_all_layouts(cx);
        root.capture_all_pane_buffers(cx);
        root.capture_all_pane_relay_ids(cx);
        manifest
            .windows
            .push(crate::persisted_terminals::PersistedWindow {
                window_id: persist_id.clone(),
                project_id: root.active_project_id(),
            });
        if settings_repo.is_none() {
            settings_repo = Some(root.settings_repo().clone());
        }
    }
    if let Some(repo) = settings_repo {
        crate::project_panes_factory::save_windows_manifest(&repo, &manifest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_window_is_main_then_sequential() {
        // The first window MUST mint "main": it's the legacy single-window
        // persistence key and the migration default, so existing state
        // restores unchanged. Later windows get unique "w{n}" ids.
        let mut reg = WindowRegistry::default();
        assert_eq!(reg.mint_persist_id(), PRIMARY_WINDOW_ID);
        assert_eq!(reg.mint_persist_id(), "w1");
        assert_eq!(reg.mint_persist_id(), "w2");
    }

    #[test]
    fn note_restored_advances_counter_so_new_windows_dont_alias() {
        // After restoring windows from a manifest, a fresh Cmd+N window must
        // never re-mint an id already in use — that would alias another
        // window's persisted rows. note_restored bumps the counter past every
        // restored id.
        let mut reg = WindowRegistry::default();
        for id in ["main", "w1", "w2"] {
            reg.note_restored(id);
        }
        assert_eq!(reg.mint_persist_id(), "w3");

        // Gaps are tolerated: the counter clears the highest seen index, and
        // "main" is never re-minted once any window exists.
        let mut reg = WindowRegistry::default();
        for id in ["w1", "w3"] {
            reg.note_restored(id);
        }
        assert_eq!(reg.mint_persist_id(), "w4");

        // Unrecognized ids are ignored (no panic, no counter change).
        let mut reg = WindowRegistry::default();
        reg.note_restored("garbage");
        assert_eq!(reg.mint_persist_id(), PRIMARY_WINDOW_ID);
    }

    #[test]
    fn ids_stay_unique_and_never_reuse_main() {
        // The counter only grows, so even after the conceptual "first" window
        // closes, a later window never re-mints "main" (which would collide
        // with persisted rows). Minted ids must all be distinct.
        let mut reg = WindowRegistry::default();
        let ids: Vec<String> = (0..5).map(|_| reg.mint_persist_id()).collect();
        assert_eq!(ids[0], PRIMARY_WINDOW_ID);
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "minted ids must be unique");
    }

    // ---------------------------------------------------------------------------
    // Pending tear-off mailbox tests — pure logic, no GPUI context needed.
    // ---------------------------------------------------------------------------

    fn make_tearoff(dest: &str, external_id: &str) -> PendingTearOff {
        PendingTearOff {
            dest_window_id: dest.to_string(),
            leaves: vec![PendingLeaf {
                external_id: external_id.to_string(),
            }],
            label: SharedString::from(format!("Terminal for {dest}")),
            color: None,
            custom_title: None,
        }
    }

    #[test]
    fn pending_tearoff_push_and_consume_by_window_id() {
        // Push two entries with distinct destination ids; verify each is
        // retrieved by its own id and the store is left empty.
        let entry_a = make_tearoff("w10", "pty-aaa");
        let entry_b = make_tearoff("w11", "pty-bbb");
        push_pending_tearoff(entry_a);
        push_pending_tearoff(entry_b);

        let got_b = consume_pending_tearoff("w11");
        assert!(got_b.is_some(), "w11 entry should be present");
        let got_b = got_b.unwrap();
        assert_eq!(got_b.dest_window_id, "w11");
        assert_eq!(got_b.leaves[0].external_id, "pty-bbb");

        let got_a = consume_pending_tearoff("w10");
        assert!(got_a.is_some(), "w10 entry should be present");
        let got_a = got_a.unwrap();
        assert_eq!(got_a.dest_window_id, "w10");
        assert_eq!(got_a.leaves[0].external_id, "pty-aaa");

        // Both consumed; further calls return None.
        assert!(consume_pending_tearoff("w10").is_none());
        assert!(consume_pending_tearoff("w11").is_none());
    }

    #[test]
    fn pending_tearoff_consume_unknown_returns_none() {
        // Consuming an id that was never pushed is a no-op (returns None,
        // does not panic).
        let result = consume_pending_tearoff("w_nonexistent_9999");
        assert!(result.is_none());
    }

    #[test]
    fn pending_tearoff_consume_is_destructive() {
        // A second consume of the same id returns None — the entry is gone.
        let entry = make_tearoff("w20", "pty-once");
        push_pending_tearoff(entry);
        let first = consume_pending_tearoff("w20");
        assert!(first.is_some());
        let second = consume_pending_tearoff("w20");
        assert!(second.is_none(), "second consume must return None");
    }
}
