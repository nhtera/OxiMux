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

use gpui::{App, Entity, Global, WindowId};

use crate::workspace_root::WorkspaceRoot;

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
}
