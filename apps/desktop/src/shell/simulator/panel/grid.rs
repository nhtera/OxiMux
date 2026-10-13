//! The multi-device grid: up to [`MAX_TILES`] devices side by side — the
//! active worktree's (always first) and those of other worktrees the user
//! added with the header's "+".
//!
//! Each tile is a viewer of its worktree's device (see
//! `oximux_simulator::viewers`): shown while the panel is, at full quality
//! while focused, lowered otherwise. One tile is focused (accent border)
//! and takes touches, keys and ⌥-inspect; clicking another only focuses
//! it. The toolbar, the consent questions and flow recording stay with the
//! active worktree's device. A tile's viewer lives as long as the tile: ×,
//! its worktree losing its device, or the window closing releases it.

use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, Subscription, Window, canvas, div, prelude::FluentBuilder as _, px,
};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::{
    Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
};
use oximux_simulator::registry::Phase;
use oximux_simulator::viewers::{StreamProfile, Viewer, ViewerId};
use oximux_simulator::DeviceId;

use super::SimulatorPanel;
use crate::shell::simulator::screen::{Binding, ScreenEvent, ScreenView};

/// Tiles in the grid, the active worktree's included.
pub(super) const MAX_TILES: usize = 4;
/// Below twice this width, tiles stack in one column.
const MIN_TILE_W: f32 = 220.0;

/// Another worktree's device in the grid.
pub(super) struct Tile {
    pub viewer: ViewerId,
    pub worktree: PathBuf,
    screen: Option<Entity<ScreenView>>,
    _events: Option<Subscription>,
}

/// How many columns `n` tiles take in a grid `width` wide: one tile fills
/// it, and a grid too narrow for two tiles of [`MIN_TILE_W`] stacks.
pub(super) fn columns(n: usize, width: f32) -> usize {
    if n <= 1 || width < 2.0 * MIN_TILE_W { 1 } else { 2 }
}

/// A worktree's short name for a tile's header.
pub(super) fn label(worktree: &Path) -> String {
    worktree.file_name().map_or_else(|| worktree.display().to_string(), |n| n.to_string_lossy().into_owned())
}

impl SimulatorPanel {
    /// Whether the grid shows (more than the active device).
    pub(super) fn has_grid(&self) -> bool {
        !self.tiles.is_empty()
    }

    /// Worktrees whose device could be added as a tile: attached, not the
    /// active one, not already in the grid — while the grid has room.
    pub(super) fn tile_candidates(&self, cx: &App) -> Vec<(PathBuf, DeviceId)> {
        let Some(hub) = self.hub.as_ref() else { return Vec::new() };
        if self.tiles.len() + 1 >= MAX_TILES {
            return Vec::new();
        }
        hub.read(cx)
            .attachments()
            .into_iter()
            .filter(|(w, _)| Some(w) != self.worktree_key.as_ref() && !self.tiles.iter().any(|t| &t.worktree == w))
            .collect()
    }

    pub(super) fn add_tile(&mut self, worktree: PathBuf, cx: &mut Context<Self>) {
        let Some(hub) = self.hub.clone() else { return };
        if !self.tile_candidates(cx).iter().any(|(w, _)| w == &worktree) {
            return;
        }
        let viewer = ViewerId::Tile(hub.update(cx, |hub, _| hub.next_viewer()));
        self.tiles.push(Tile { viewer, worktree, screen: None, _events: None });
        self.sync_viewers(cx);
        cx.notify();
    }

    pub(super) fn remove_tile(&mut self, viewer: ViewerId, cx: &mut Context<Self>) {
        let Some(at) = self.tiles.iter().position(|t| t.viewer == viewer) else { return };
        let tile = self.tiles.remove(at);
        if let Some(hub) = self.hub.clone() {
            hub.update(cx, |hub, cx| hub.release_viewer(&tile.worktree, tile.viewer, cx));
        }
        if self.focused == Some(viewer) || self.tiles.is_empty() {
            self.focused = None;
        }
        self.sync_viewers(cx);
        cx.notify();
    }

    /// The active worktree is becoming `next`: if `next` has a tile, the
    /// device that was active takes its place (the user sees both still),
    /// and the new active one is focused. The old one's tile viewer is added
    /// here; the replaced tile's viewer is returned, for the caller to
    /// release once the panel's own viewer shows `next`.
    pub(super) fn swap_into_tile(&mut self, next: Option<&Path>, cx: &mut Context<Self>) -> Option<(PathBuf, ViewerId)> {
        let (hub, next, old) = (self.hub.clone()?, next?, self.worktree_key.clone()?);
        let at = self.tiles.iter().position(|t| t.worktree == next)?;
        let replaced = (next.to_path_buf(), self.tiles[at].viewer);
        if hub.read(cx).device_for(&old).is_some() {
            let viewer = ViewerId::Tile(hub.update(cx, |hub, _| hub.next_viewer()));
            let shown = Viewer { shown: self.shown, profile: StreamProfile::Reduced };
            hub.update(cx, |hub, cx| hub.set_viewer(&old, viewer, shown, cx));
            self.tiles[at] = Tile { viewer, worktree: old, screen: None, _events: None };
        } else {
            self.tiles.remove(at);
        }
        self.focused = None;
        Some(replaced)
    }

    /// The devices this panel shows: the active worktree's and its tiles'.
    pub(super) fn shown_devices(&self, cx: &App) -> Vec<DeviceId> {
        let Some(hub) = self.hub.as_ref().map(|h| h.read(cx)) else { return Vec::new() };
        self.device(cx).into_iter().chain(self.tiles.iter().filter_map(|t| hub.device_for(&t.worktree))).collect()
    }

    /// Drop tiles that no longer show anything of their own: the worktree
    /// lost its device, or became the active one.
    pub(super) fn prune_tiles(&mut self, cx: &mut Context<Self>) {
        let Some(hub) = self.hub.clone() else { return };
        let gone: Vec<ViewerId> = self
            .tiles
            .iter()
            .filter(|t| Some(&t.worktree) == self.worktree_key.as_ref() || hub.read(cx).device_for(&t.worktree).is_none())
            .map(|t| t.viewer)
            .collect();
        for viewer in gone {
            self.remove_tile(viewer, cx);
        }
    }

    /// Release every tile's viewer (the window is closing).
    pub(super) fn release_tiles(&mut self, cx: &mut App) {
        let Some(hub) = self.hub.clone() else { return };
        for tile in self.tiles.drain(..) {
            hub.update(cx, |hub, cx| hub.release_viewer(&tile.worktree, tile.viewer, cx));
        }
    }

    /// Focus a tile (`None`: the active worktree's).
    pub(super) fn focus_tile(&mut self, viewer: Option<ViewerId>, cx: &mut Context<Self>) {
        if self.focused != viewer {
            self.focused = viewer;
            self.sync_viewers(cx);
            cx.notify();
        }
    }

    /// The panel's own viewer: lowered while another tile has the focus.
    pub(super) fn panel_viewer(&self) -> Viewer {
        let profile = if self.has_grid() && self.focused.is_some() { StreamProfile::Reduced } else { StreamProfile::Full };
        Viewer { shown: true, profile }
    }

    /// Tell the hub what each viewer of this panel looks like now.
    pub(super) fn sync_viewers(&mut self, cx: &mut Context<Self>) {
        let Some(hub) = self.hub.clone() else { return };
        let (shown, focused, panel, own) = (self.shown, self.focused, self.panel_viewer(), self.own);
        let worktree = self.worktree.clone();
        let tiles: Vec<_> = self.tiles.iter().map(|t| (t.worktree.clone(), t.viewer)).collect();
        hub.update(cx, |hub, cx| {
            if let Some(worktree) = worktree.as_ref().filter(|_| shown) {
                hub.set_viewer(worktree, own, panel, cx);
            }
            for (worktree, viewer) in tiles {
                let profile = if focused == Some(viewer) { StreamProfile::Full } else { StreamProfile::Reduced };
                hub.set_viewer(&worktree, viewer, Viewer { shown, profile }, cx);
            }
        });
    }

    /// Stop decoding in every tile (the panel went out of sight).
    pub(super) fn hide_tiles(&mut self, cx: &mut Context<Self>) {
        for screen in self.tiles.iter().filter_map(|t| t.screen.clone()) {
            screen.update(cx, |screen, cx| screen.hide(cx));
        }
    }

    /// The grid: the active worktree's tile (showing `active`, what its
    /// state puts on screen) first, then the others.
    pub(super) fn render_grid(&mut self, active: AnyElement, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let gap = self.density.pad_panel;
        let n = self.tiles.len() + 1;
        let cols = columns(n, self.grid_width.get().unwrap_or(0.0));
        let mut cells = vec![self.render_tile(None, active, cx)];
        for i in 0..self.tiles.len() {
            // A question its agent is waiting on comes first: the user is
            // looking at that device here.
            let worktree = self.tiles[i].worktree.clone();
            let content = match self.render_consent_for(&worktree, cx) {
                Some(question) => div().flex().size_full().items_center().justify_center().p(px(gap)).child(question).into_any_element(),
                None => self.tile_content(i, window, cx),
            };
            cells.push(self.render_tile(Some(self.tiles[i].viewer), content, cx));
        }
        let mut rows = Vec::new();
        let mut cells = cells.into_iter().peekable();
        while cells.peek().is_some() {
            let row: Vec<AnyElement> = cells.by_ref().take(cols).collect();
            let fill = cols - row.len();
            rows.push(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.))
                    .w_full()
                    .gap(px(gap))
                    .children(row)
                    // A short last row keeps its tiles at the others' width.
                    .children((0..fill).map(|_| div().flex_1().min_w(px(0.)))),
            );
        }
        // Stacked three or four deep, each tile is small: say how to fix it.
        let hint = (cols == 1 && n > 2 && self.grid_width.get().is_some()).then(|| {
            div()
                .w_full()
                .flex_none()
                .text_center()
                .text_size(px(self.typography.t_body_sm))
                .text_color(self.theme.fg_subtle)
                .child("Widen the sidebar to see the devices side by side.")
        });
        let measured = self.grid_width.clone();
        let panel = cx.weak_entity();
        let meter = canvas(
            move |bounds, _window, cx| {
                let width = f32::from(bounds.size.width);
                if measured.replace(Some(width)) != Some(width) {
                    // Too late for this frame: lay out again after it.
                    cx.defer(move |cx| {
                        let _ = panel.update(cx, |_, cx| cx.notify());
                    });
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .gap(px(gap))
            .child(meter)
            .children(hint)
            .children(rows)
            .into_any_element()
    }

    /// What tile `i` shows: its device's screen while it streams, else a line
    /// saying where it stands.
    fn tile_content(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(hub) = self.hub.clone() else { return self.centered_line("") };
        let (worktree, viewer) = (self.tiles[i].worktree.clone(), self.tiles[i].viewer);
        let device = hub.read(cx).device_for(&worktree);
        let phase = device.as_ref().map_or(Phase::Idle, |d| hub.read(cx).phase(d));
        // What it says when not streaming, and the button that starts it.
        let (line, action) = match &phase {
            Phase::Live { .. } => (None, None),
            Phase::Booting { .. } => (Some("Booting…".to_owned()), None),
            Phase::Starting { .. } | Phase::Parked => (Some("Starting stream…".to_owned()), None),
            Phase::Disconnected { reason } => (Some(reason.clone()), Some(TileAction::Reconnect)),
            Phase::Failed { error } => (Some(error.clone()), Some(TileAction::Reconnect)),
            // Attached before a restart: it starts when someone asks.
            Phase::Idle => (Some("Not streaming.".to_owned()), Some(TileAction::Start)),
        };
        if let Some(line) = line {
            return self.tile_stopped(&line, worktree, device, action, cx);
        }
        let (theme, typography) = (self.theme, self.typography.clone());
        let screen = match self.tiles[i].screen.clone() {
            Some(screen) => screen,
            None => {
                let screen = cx.new(|cx| ScreenView::new(hub, theme, typography.clone(), window, cx));
                let events = cx.subscribe(&screen, move |panel, _, event: &ScreenEvent, cx| {
                    if matches!(event, ScreenEvent::Focus) {
                        panel.focus_tile(Some(viewer), cx);
                    }
                });
                self.tiles[i].screen = Some(screen.clone());
                self.tiles[i]._events = Some(events);
                screen
            }
        };
        let focused = self.focused == Some(viewer);
        let binding = Binding {
            device,
            visible: self.visible && self.window_visible,
            radius: 0.0,
            focused,
            // A pick is sent to the active worktree's agent: only its device.
            inspect: false,
            throttle: !focused,
        };
        screen.update(cx, |screen, cx| {
            screen.bind(binding, theme, &typography, window, cx);
            screen.set_recording(None, cx);
        });
        screen.into_any_element()
    }

    /// A tile that is not streaming: why, and how to start it.
    fn tile_stopped(&self, line: &str, worktree: PathBuf, device: Option<DeviceId>, action: Option<TileAction>, cx: &mut Context<Self>) -> AnyElement {
        let (theme, density, ty) = (self.theme, self.density, &self.typography);
        let button = action.zip(device).map(|(action, udid)| {
            let label = match action {
                TileAction::Start => "Start",
                TileAction::Reconnect => "Reconnect",
            };
            Button::new(("sim-tile-start", tile_key_of(&worktree)))
                .outline()
                .small()
                .label(label)
                .on_click(cx.listener(move |panel, _, _window, cx| {
                    let Some(hub) = panel.hub.clone() else { return };
                    hub.update(cx, |hub, cx| match action {
                        // Attaching the device it already has starts it.
                        TileAction::Start => hub.attach(&worktree, Some(udid.clone()), None, cx),
                        TileAction::Reconnect => {
                            hub.clear_stopped_by_user(&udid);
                            hub.reconnect(&udid, cx);
                        }
                    });
                }))
        });
        div()
            .flex()
            .flex_col()
            .size_full()
            .items_center()
            .justify_center()
            .gap(px(density.pad_panel))
            .p(px(density.pad_panel))
            .child(div().w_full().text_center().text_size(px(ty.t_body_md)).text_color(theme.fg_muted).child(line.to_owned()))
            .children(button)
            .into_any_element()
    }

    /// One tile: a header (worktree · device, the agents' badge, ×) over its
    /// screen, outlined in the accent colour while focused.
    fn render_tile(&self, viewer: Option<ViewerId>, content: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        let (theme, density, ty) = (self.theme, self.density, &self.typography);
        let focused = self.focused == viewer;
        let worktree = match viewer {
            None => self.worktree.clone(),
            Some(v) => self.tiles.iter().find(|t| t.viewer == v).map(|t| t.worktree.clone()),
        };
        let hub = self.hub.as_ref().map(|h| h.read(cx));
        let device = worktree.as_ref().zip(hub).and_then(|(w, hub)| hub.device_for(w));
        let name = device
            .as_ref()
            .and_then(|d| self.devices(cx).into_iter().find(|i| &i.udid == d))
            .map(|i| i.name)
            // Not listed (a phone unplugged): its id, which still says which.
            .unwrap_or_else(|| device.as_ref().map_or_else(|| "No device".to_owned(), ToString::to_string));
        let (allowed, active) = device.as_ref().zip(self.hub.as_ref()).map_or((false, false), |(d, hub)| {
            let hub = hub.read(cx);
            (hub.agents_allowed(d), hub.agent_active(d))
        });
        let badge = (allowed || active).then(|| {
            div()
                .flex_none()
                .size(px(6.))
                .rounded_full()
                .bg(if active { theme.status_info } else { theme.status_ok })
        });
        let tip = if active { "An agent is using this device" } else { "Agents may control this device" };
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(density.gap_inline))
            .w_full()
            .flex_none()
            .min_w(px(0.))
            .text_size(px(ty.t_body_sm))
            .text_color(if focused { theme.fg_base } else { theme.fg_muted })
            .child(div().flex_1().min_w(px(0.)).truncate().child(format!(
                "{} · {name}",
                worktree.as_deref().map(label).unwrap_or_default()
            )))
            .children(badge.map(|b| div().id(("sim-tile-badge", tile_key(viewer))).tooltip(move |window, cx| gpui_component::tooltip::Tooltip::new(tip).build(window, cx)).child(b)))
            .when_some(viewer, |header, viewer| {
                header.child(
                    Button::new(("sim-tile-close", tile_key(Some(viewer))))
                        .ghost()
                        .xsmall()
                        .icon(Icon::default().path("icons/x.svg"))
                        .tooltip("Remove from the grid")
                        .on_click(cx.listener(move |panel, _, _window, cx| panel.remove_tile(viewer, cx))),
                )
            });
        div()
            .id(("sim-tile", tile_key(viewer)))
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .gap(px(density.gap_inline))
            .on_mouse_down(MouseButton::Left, cx.listener(move |panel, _, _window, cx| panel.focus_tile(viewer, cx)))
            .child(header)
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_1()
                    .min_h(px(0.))
                    .w_full()
                    .rounded(px(density.r_card))
                    .overflow_hidden()
                    .border_2()
                    .border_color(if focused { theme.focus_ring } else { theme.border_inactive })
                    .bg(theme.bg_panel_alt)
                    .child(content),
            )
            .into_any_element()
    }
}

impl SimulatorPanel {
    /// The header's "+": another worktree's device into the grid. Shown
    /// only while there is one to add and room for it.
    pub(super) fn render_add_tile(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let candidates = self.tile_candidates(cx);
        if candidates.is_empty() {
            return None;
        }
        let devices = self.devices(cx);
        let rows: Vec<(PathBuf, String)> = candidates
            .into_iter()
            .map(|(worktree, udid)| {
                let name = devices.iter().find(|d| d.udid == udid).map_or_else(|| udid.to_string(), |d| d.name.clone());
                let text = format!("{} · {name}", label(&worktree));
                (worktree, text)
            })
            .collect();
        let weak = cx.weak_entity();
        Some(
            Button::new("sim-add-tile")
                .ghost()
                .xsmall()
                .icon(Icon::default().path("icons/plus.svg"))
                .tooltip("Show another worktree's device")
                .dropdown_menu(move |mut menu, _window, _cx| {
                    menu = menu.label("Show beside this one");
                    for (worktree, text) in &rows {
                        let (weak, worktree) = (weak.clone(), worktree.clone());
                        menu = menu.item(PopupMenuItem::new(text.clone()).on_click(move |_, _window, cx| {
                            let _ = weak.update(cx, |panel, cx| panel.add_tile(worktree.clone(), cx));
                        }));
                    }
                    menu
                })
                .into_any_element(),
        )
    }
}

/// How a stopped tile can be started.
#[derive(Clone, Copy)]
enum TileAction {
    Start,
    Reconnect,
}

/// An element id number for a worktree's tile.
fn tile_key_of(worktree: &Path) -> usize {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    worktree.hash(&mut hasher);
    hasher.finish() as usize
}

/// A tile's number for element ids (0: the active worktree's).
fn tile_key(viewer: Option<ViewerId>) -> usize {
    match viewer {
        Some(ViewerId::Tile(n)) => n as usize,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_tile_fills_and_a_narrow_grid_stacks() {
        assert_eq!(columns(1, 1000.0), 1);
        assert_eq!(columns(2, 2.0 * MIN_TILE_W), 2, "side by side");
        assert_eq!(columns(2, 2.0 * MIN_TILE_W - 1.0), 1, "stacked when narrower");
        assert_eq!(columns(3, 600.0), 2, "2×2");
        assert_eq!(columns(4, 600.0), 2, "2×2");
        assert_eq!(columns(4, 300.0), 1, "one column in a narrow dock");
        assert_eq!(columns(2, 0.0), 1, "before the first measurement");
    }

    #[test]
    fn a_tile_is_labelled_by_its_folder() {
        assert_eq!(label(Path::new("/w/app-feature")), "app-feature");
        assert_eq!(label(Path::new("/")), "/");
    }
}
