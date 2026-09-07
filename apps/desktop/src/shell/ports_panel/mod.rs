//! Ports panel — what this machine is currently serving, and which of it is
//! yours.
//!
//! **Why this is a panel and not a log line.** A dev server prints its URL
//! once, at the moment it starts, and then a build log buries it. Ten minutes
//! later "was that 5173 or 5174?" costs a scroll-back, and after a restart on a
//! different port it costs a wrong answer. The socket outlives the line: it
//! exists for exactly as long as the server is accepting connections, so a
//! panel reading the socket table is always current in a way a transcript
//! never is.
//!
//! **Why it lists more than your terminals.** See [`scan`] for the argument
//! and the attribution rules. The short version: the panel's job is to answer
//! "what has 3000", and a list that can only see processes this window started
//! answers a narrower question than the one being asked. Everything is listed;
//! what the panel *claims* is what attribution could justify, and only claimed
//! rows get a destructive action.
//!
//! **Why labels are persisted.** Three `node` rows on 3000, 3001 and 9229 are
//! the API, the docs site and a debugger, and nothing the kernel knows can
//! tell them apart. The name is the user's to write, so it is stored against
//! project+port and comes back when the same server does.
//!
//! The scan itself is driven by [`crate::workspace_root::WorkspaceRoot`] —
//! it owns the terminals and the project list the walk starts from, and the
//! socket read belongs on a background thread. This module renders what the
//! scan produced and owns the actions on a row.

pub(crate) mod kill;
pub(crate) mod labels;
pub(crate) mod scan;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, ClipboardItem, Context, FocusHandle, Focusable,
    InteractiveElement, IntoElement, MouseButton, ParentElement, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled, WeakEntity, Window, div, prelude::FluentBuilder as _,
    px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{
    Disableable as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
};
use oximux_settings::{Density, Theme, Typography};
use oximux_storage::SettingsRepo;

use crate::app_settings::port_label_settings;
use crate::workspace_root::WorkspaceRoot;

use labels::{
    attribution_tooltip, detail_label, empty_detail, empty_headline, external_section_label,
    no_owned_hint, project_label, row_title, url_for,
};
use scan::{PortInventory, PortRow};

/// Opacity of a row's action cluster at rest.
///
/// Not zero, for the reason the stash panel's row actions document: this panel
/// has no context menu, so a fully hidden cluster would leave Stop reachable
/// only by a hover a user has no reason to try. Ghosted-at-rest keeps every
/// verb discoverable while still letting the port number and its process read
/// as the row's content.
const ACTION_REST_OPACITY: f32 = 0.35;

/// Height of the panel's header strip.
///
/// The file explorer's number, deliberately: these are sibling panels in the
/// same column, and a header that is four pixels taller than its neighbour
/// reads as a misalignment rather than as a choice. Notably *not*
/// `density.h_top_bar`, which is pinned to the macOS traffic lights and
/// belongs to the window chrome — borrowing it here is what made this header
/// overflow its own two-line title.
const HEADER_H: f32 = 28.0;

/// Which row's label is being edited. Project *and* port, because that pair is
/// the label's identity — a pid would be gone by the next poll and the row
/// would stop matching mid-edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RenameTarget {
    pub project: PathBuf,
    pub port: u16,
}

/// Which section a row lives in. Sections collapse independently and the
/// external one starts closed, so the key has to survive a poll that reorders
/// nothing but rebuilds everything.
fn project_section_key(project: &std::path::Path) -> String {
    format!("project:{}", project.display())
}

const EXTERNAL_SECTION_KEY: &str = "external";

pub struct PortsPanel {
    weak_root: WeakEntity<WorkspaceRoot>,
    focus_handle: FocusHandle,
    /// `None` in tests; a panel without a store simply never persists.
    settings_repo: Option<SettingsRepo>,

    inventory: PortInventory,
    /// Persisted labels by [`port_label_settings::label_key`]. Loaded once and
    /// updated in place on write, so a poll never touches the database.
    port_labels: HashMap<String, String>,
    /// Sections the user has closed. External is in here from the start: it is
    /// long, it is mostly the OS talking to itself, and it is reference
    /// material rather than the thing the panel was opened for.
    collapsed: HashSet<String>,
    /// Set while a *manual* refresh is in flight, so the button can say so.
    ///
    /// Only manual: the background poll runs every few seconds forever, and a
    /// spinner driven by it would repaint the window on that cadence for as
    /// long as the app is open — a battery bug wearing a feature's clothes.
    refreshing: bool,

    rename: Option<RenameTarget>,
    rename_input: gpui::Entity<InputState>,
    _rename_subscription: gpui::Subscription,

    theme: Theme,
    density: Density,
    typography: Typography,
    scroll: ScrollHandle,
}

impl PortsPanel {
    pub fn new(
        weak_root: WeakEntity<WorkspaceRoot>,
        settings_repo: Option<SettingsRepo>,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let port_labels = settings_repo
            .as_ref()
            .map(port_label_settings::load_labels)
            .unwrap_or_default();
        let rename_input = cx.new(|cx| InputState::new(window, cx).placeholder("Name this port"));
        // Enter commits. Without this the only way out of an edit is a click,
        // and a text field that ignores Enter reads as broken.
        let subscription = cx.subscribe_in(
            &rename_input,
            window,
            |this: &mut Self, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.commit_rename(window, cx);
                }
            },
        );
        Self {
            weak_root,
            focus_handle: cx.focus_handle(),
            settings_repo,
            inventory: PortInventory::default(),
            port_labels,
            collapsed: HashSet::from([EXTERNAL_SECTION_KEY.to_string()]),
            refreshing: false,
            rename: None,
            rename_input,
            _rename_subscription: subscription,
            theme,
            density,
            typography,
            scroll: ScrollHandle::new(),
        }
    }

    /// Install a freshly scanned inventory.
    ///
    /// Repaints only when something actually changed: this is called on a
    /// cadence, and a panel that invalidates the window every few seconds
    /// forever is a battery bug wearing a feature's clothes. The one exception
    /// is a manual refresh landing — clearing that flag *is* a change, and it
    /// is what makes the button stop saying it is working.
    pub fn apply(&mut self, inventory: PortInventory, cx: &mut Context<Self>) {
        let settling = self.refreshing;
        self.refreshing = false;
        if self.inventory == inventory && !settling {
            return;
        }
        // An edit whose row has gone is an edit with nowhere to commit to.
        if let Some(target) = &self.rename
            && !inventory.groups.iter().any(|g| {
                g.project == target.project && g.rows.iter().any(|r| r.port == target.port)
            })
        {
            self.rename = None;
        }
        self.inventory = inventory;
        cx.notify();
    }

    /// Total project-attributed rows, for the status bar's metric.
    pub fn count(&self) -> usize {
        self.inventory.total()
    }

    fn label_for(&self, project: &std::path::Path, port: u16) -> Option<&str> {
        self.port_labels
            .get(&port_label_settings::label_key(project, port))
            .map(String::as_str)
    }

    fn toggle_section(&mut self, key: String, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
        cx.notify();
    }

    fn begin_rename(
        &mut self,
        project: PathBuf,
        port: u16,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.label_for(&project, port).unwrap_or_default().to_string();
        self.rename_input.update(cx, |state, cx| {
            state.set_value(current, window, cx);
        });
        self.rename = Some(RenameTarget { project, port });
        // Deferred, for two reasons that both bite here. A synchronous focus
        // inside a click handler is clobbered by the click's own
        // post-dispatch focus pass (the same trap `question_card::focus_self`
        // documents), and the field does not exist yet at this point — the row
        // was rendering a title a moment ago, and the input only appears on
        // the paint this call triggers. The deferred pass runs after both
        // settle, so the caret lands and the Rename click is one click.
        let handle = self.rename_input.focus_handle(cx);
        window.defer(cx, move |window, cx| handle.focus(window, cx));
        cx.notify();
    }

    fn commit_rename(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.rename.take() else {
            return;
        };
        let typed = self.rename_input.read(cx).value().to_string();
        let key = port_label_settings::label_key(&target.project, target.port);
        // The in-memory map is the render's source of truth, so it is updated
        // whether or not there is a store to persist to — a panel mounted
        // without one still shows the label for as long as it lives.
        match port_label_settings::normalize(&typed) {
            Some(label) => {
                self.port_labels.insert(key, label);
            }
            None => {
                self.port_labels.remove(&key);
            }
        }
        if let Some(repo) = &self.settings_repo {
            port_label_settings::save_label(repo, &target.project, target.port, &typed);
        }
        cx.notify();
    }

    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        self.rename = None;
        cx.notify();
    }

    /// Open the port in the user's default browser.
    ///
    /// The system browser rather than the app's own browser pane — see
    /// [`crate::shell::open_url::open_loopback_port`] for why routing this
    /// through the embedded webview would crash the process on Windows. It is
    /// also the better answer on its own merits: a dev server is usually being
    /// opened *next to* devtools and an existing logged-in session.
    fn open_port(&mut self, port: u16, cx: &mut Context<Self>) {
        crate::shell::open_url::open_loopback_port(port, cx);
    }

    fn copy_url(&mut self, port: u16, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(url_for(port)));
        self.toast(
            crate::shell::toast::ToastKind::Info,
            format!("Copied {}", url_for(port)),
            cx,
        );
    }

    /// Stop the process behind a row, then rescan so the row goes away.
    ///
    /// Only ever reached from an owned row — see [`kill`] for why external
    /// rows are not offered this, and for the staleness re-check that makes a
    /// click on a several-seconds-old row safe.
    fn stop_port(&mut self, pid: u32, port: u16, cx: &mut Context<Self>) {
        let spawner = {
            let executor = cx.background_executor().clone();
            move |delay: std::time::Duration, escalate: Box<dyn FnOnce() + Send>| {
                let timer = executor.timer(delay);
                executor
                    .spawn(async move {
                        // The grace wait happens here rather than on the UI
                        // thread; nothing is holding a click open for it.
                        timer.await;
                        escalate();
                    })
                    .detach();
            }
        };
        match kill::stop_listener(pid, port, spawner) {
            Ok(()) => {
                // Deliberately no immediate rescan. The socket does not close
                // the instant SIGTERM lands, so a scan fired from this click
                // would redraw the row it was meant to remove and read as a
                // failed stop; the toast is what acknowledges the click, and
                // the ordinary poll is what retires the row a moment later.
                self.toast(
                    crate::shell::toast::ToastKind::Info,
                    format!("Stopping the process on {port}"),
                    cx,
                );
            }
            Err(refusal) => {
                self.toast(
                    crate::shell::toast::ToastKind::Error,
                    refusal.message(port),
                    cx,
                );
            }
        }
    }

    fn toast(&self, kind: crate::shell::toast::ToastKind, text: String, cx: &mut Context<Self>) {
        let _ = self.weak_root.update(cx, |root, cx| {
            root.push_toast(kind, text, cx);
        });
    }

    /// Ask the root for an out-of-cadence scan.
    ///
    /// The busy flag is set only if the root actually took the request. A
    /// panel whose root has gone — a closed window's, mid-teardown — would
    /// otherwise disable its own button forever waiting for a scan nobody is
    /// running.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let asked = self
            .weak_root
            .update(cx, |root, cx| {
                root.run_port_scan(cx);
            })
            .is_ok();
        if asked {
            self.refreshing = true;
            cx.notify();
        }
    }
}

impl Focusable for PortsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PortsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(
            &mut self.theme,
            &mut self.density,
            &mut self.typography,
            cx,
        );
        let theme = self.theme;
        let density = self.density;

        let body: AnyElement = if self.inventory.is_empty() {
            self.render_empty()
        } else {
            let mut col = div()
                .id("ports-list")
                .flex()
                .flex_col()
                .w_full()
                .flex_1()
                .min_h(px(0.))
                .pb(px(density.pad_panel))
                .overflow_y_scroll()
                .track_scroll(&self.scroll);
            if self.inventory.groups.is_empty() {
                // Plenty listening, none of it yours — a different fact from
                // "nothing is listening", and the one that means a dev server
                // failed to start.
                col = col.child(self.render_hint(no_owned_hint()));
            }
            for group in 0..self.inventory.groups.len() {
                col = col.child(self.render_project_section(group, cx));
            }
            if !self.inventory.external.is_empty() {
                col = col.child(self.render_external_section(cx));
            }
            col.into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .h_full()
            .w_full()
            .bg(theme.bg_panel)
            .child(self.render_header(cx))
            .child(body)
    }
}

impl PortsPanel {
    fn render_header(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let typography = self.typography.clone();
        let refreshing = self.refreshing;

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(8.0))
            .w_full()
            .flex_none()
            .h(px(HEADER_H))
            .pl(px(10.0))
            .pr(px(4.0))
            .bg(theme.bg_panel)
            .border_b_1()
            .border_color(theme.border_inactive)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .text_size(px(typography.t_label_caps))
                    .font_weight(typography.w_semibold)
                    .text_color(theme.fg_muted)
                    .child("PORTS"),
            )
            .child(
                Button::new("ports-refresh")
                    .ghost()
                    .xsmall()
                    .icon(Icon::default().path("icons/refresh-cw.svg"))
                    // The tooltip is the only place this control is named; an
                    // icon button with none is a mystery glyph.
                    .tooltip(if refreshing {
                        "Scanning ports…"
                    } else {
                        "Rescan ports"
                    })
                    // Disabled while a manual scan is in flight, because the
                    // root refuses an overlapping scan silently — a button
                    // that accepts a click and does nothing is worse than one
                    // that says it is busy.
                    .disabled(refreshing)
                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| this.refresh(cx))),
            )
            .into_any_element()
    }

    fn render_empty(&self) -> AnyElement {
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .items_center()
            .justify_center()
            .gap(px(6.0))
            .p(px(density.pad_panel))
            .child(
                div()
                    .text_size(px(typography.t_body_md))
                    .text_color(theme.fg_muted)
                    .child(empty_headline()),
            )
            .child(
                div()
                    .text_size(px(typography.t_sub_label))
                    .text_color(theme.fg_subtle)
                    .text_center()
                    .child(empty_detail()),
            )
            .into_any_element()
    }

    /// A single recessive line of explanation between the header and a section.
    fn render_hint(&self, text: &'static str) -> AnyElement {
        let theme = self.theme;
        let density = self.density;
        div()
            .w_full()
            .px(px(density.pad_panel))
            .pt(px(density.pad_row))
            .text_size(px(self.typography.t_sub_label))
            .text_color(theme.fg_subtle)
            .child(text)
            .into_any_element()
    }

    /// A collapsible section heading: chevron, title, and the count of what is
    /// inside it.
    ///
    /// The count is on the heading rather than in the panel header because
    /// there are now several numbers to report and one summary line cannot
    /// hold them without becoming the thing that overflowed this header
    /// before. It also keeps the number next to the thing it counts.
    fn render_section_header(
        &self,
        key: String,
        title: String,
        count: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let collapsed = self.collapsed.contains(&key);
        let id = SharedString::from(format!("ports-section-{key}"));
        let toggle_key = key.clone();

        div()
            .id(id)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .w_full()
            .h(px(density.h_row))
            .px(px(density.pad_panel))
            .cursor_pointer()
            .hover(|s| s.bg(theme.hover_overlay))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _window, cx| {
                    this.toggle_section(toggle_key.clone(), cx)
                }),
            )
            .child(
                Icon::default()
                    .path(if collapsed {
                        "icons/chevron-right.svg"
                    } else {
                        "icons/chevron-down.svg"
                    })
                    .xsmall()
                    // An svg paints transparent without one: see
                    // `oximux-app-svg-icons-need-assets-registration`.
                    .text_color(theme.fg_subtle),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .text_size(px(typography.t_label_caps))
                    .font_weight(typography.w_semibold)
                    .text_color(theme.fg_muted)
                    .child(title),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(typography.t_sub_label))
                    .text_color(theme.fg_subtle)
                    .child(count.to_string()),
            )
            .into_any_element()
    }

    fn render_project_section(&mut self, idx: usize, cx: &mut Context<Self>) -> AnyElement {
        let project = self.inventory.groups[idx].project.clone();
        let key = project_section_key(&project);
        let count = self.inventory.groups[idx].rows.len();
        let mut col = div().flex().flex_col().w_full().child(
            self.render_section_header(key.clone(), project_label(&project).to_uppercase(), count, cx),
        );
        if !self.collapsed.contains(&key) {
            for row in 0..count {
                col = col.child(self.render_row(Some((idx, project.clone())), row, cx));
            }
        }
        col.into_any_element()
    }

    fn render_external_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let key = EXTERNAL_SECTION_KEY.to_string();
        let count = self.inventory.external.len();
        let mut col = div().flex().flex_col().w_full().child(self.render_section_header(
            key.clone(),
            external_section_label().to_string(),
            count,
            cx,
        ));
        if !self.collapsed.contains(&key) {
            for row in 0..count {
                col = col.child(self.render_row(None, row, cx));
            }
        }
        col.into_any_element()
    }

    /// One port. `owner` is `Some((group index, project))` for an attributed
    /// row and `None` for an external one — the two differ in which actions
    /// they carry, not in how they look.
    fn render_row(
        &mut self,
        owner: Option<(usize, PathBuf)>,
        row: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let entry: &PortRow = match &owner {
            Some((group, _)) => &self.inventory.groups[*group].rows[row],
            None => &self.inventory.external[row],
        };
        let port = entry.port;
        let pid = entry.pid;
        let process = entry.process.clone();
        let loopback = entry.loopback;
        let attribution = entry.attribution;
        let owned = entry.is_owned();
        let title = match &owner {
            Some((_, project)) => row_title(self.label_for(project, port), &process, port),
            None => row_title(None, &process, port),
        };
        let editing = owner.as_ref().is_some_and(|(_, project)| {
            self.rename
                .as_ref()
                .is_some_and(|t| t.port == port && &t.project == project)
        });

        // Element ids must be unique across the whole panel, and a port number
        // alone is not: two projects can each serve 3000 only if one of them
        // has exited, but the panel may render both in the frame between. The
        // external section shares the id space, hence the section prefix.
        let key = match &owner {
            Some((group, _)) => format!("g{group}-{row}"),
            None => format!("x-{row}"),
        };
        let group_name = SharedString::from(format!("ports-row-{key}"));

        let mut actions = div()
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .gap(px(1.0))
            .opacity(ACTION_REST_OPACITY)
            .group_hover(group_name.clone(), |s| s.opacity(1.0))
            .child(
                Button::new(SharedString::from(format!("port-open-{key}")))
                    .ghost()
                    .xsmall()
                    .icon(Icon::default().path("icons/globe.svg"))
                    .tooltip(SharedString::from(format!("Open {}", url_for(port))))
                    .on_click(
                        cx.listener(move |this, _: &ClickEvent, _w, cx| this.open_port(port, cx)),
                    ),
            )
            .child(
                Button::new(SharedString::from(format!("port-copy-{key}")))
                    .ghost()
                    .xsmall()
                    .icon(Icon::default().path("icons/copy.svg"))
                    .tooltip(SharedString::from(format!("Copy {}", url_for(port))))
                    .on_click(
                        cx.listener(move |this, _: &ClickEvent, _w, cx| this.copy_url(port, cx)),
                    ),
            );
        if let Some((_, project)) = owner.clone() {
            actions = actions.child(
                Button::new(SharedString::from(format!("port-rename-{key}")))
                    .ghost()
                    .xsmall()
                    .icon(Icon::default().path("icons/pencil.svg"))
                    .tooltip("Name this port")
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.begin_rename(project.clone(), port, window, cx)
                    })),
            );
        }
        if owned {
            actions = actions
                // A gap, not a confirmation step. Stop is one click from Copy
                // in a cluster of small icons, and an accidental SIGTERM to a
                // dev server is a real cost — but it is a recoverable one
                // (restart it), so the proportionate answer is to stop the
                // destructive verb sitting flush against the harmless ones
                // rather than to put a dialog in front of every deliberate
                // use.
                .child(div().flex_none().w(px(density.gap_inline)))
                .child(
                    Button::new(SharedString::from(format!("port-stop-{key}")))
                        .ghost()
                        .xsmall()
                        .icon(
                            Icon::default()
                                .path("icons/trash.svg")
                                .text_color(theme.status_error),
                        )
                        .tooltip("Stop this process")
                        .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
                            this.stop_port(pid, port, cx)
                        })),
                );
        }

        let headline: AnyElement = if editing {
            div()
                // `flex_1` claims the free space; `min_w_0` lets the field
                // shrink instead of forcing the row wider than the sidebar.
                .flex_1()
                .min_w_0()
                .child(Input::new(&self.rename_input).small())
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_base)
                .child(title)
                .into_any_element()
        };

        div()
            // Stateful unconditionally: `hover` needs an id to key its state
            // on, and an id that only appears on some rows would make the
            // hover highlight arrive and leave with the attribution.
            .id(SharedString::from(format!("port-row-{key}")))
            .group(group_name)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(density.gap_inline))
            .w_full()
            .py(px(density.pad_row))
            .pl(px(density.pad_panel))
            .pr(px(4.0))
            .hover(|s| s.bg(theme.hover_overlay))
            // Why this row is filed here, on the row rather than in a legend.
            .when_some(attribution, |row, how| {
                let tip = SharedString::from(attribution_tooltip(how));
                row.tooltip(move |window, cx| {
                    gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                })
            })
            .child(
            // The port number leads the row: it is what the user came to read,
            // it is fixed-width, and it is the only field that is never empty.
            div()
                .flex_none()
                .text_size(px(typography.t_body_sm))
                .font_weight(typography.w_semibold)
                .text_color(theme.fg_base)
                .child(format!(":{port}")),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap(px(1.0))
                .child(headline)
                .when(!editing, |col| {
                    col.child(
                        div()
                            .text_size(px(typography.t_sub_label))
                            // A server reachable from the network is the
                            // surprising case, so it is the one that gets a
                            // colour rather than the recessive grey.
                            .text_color(if loopback {
                                theme.fg_subtle
                            } else {
                                theme.status_warn
                            })
                            .overflow_hidden()
                            .child(detail_label(&process, pid, loopback)),
                    )
                }),
        )
        .when(editing, |row| {
            row.child(
                Button::new(SharedString::from(format!("port-save-{key}")))
                    .ghost()
                    .xsmall()
                    .icon(Icon::default().path("icons/check.svg"))
                    .tooltip("Save name")
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.commit_rename(window, cx)
                    })),
            )
            .child(
                Button::new(SharedString::from(format!("port-cancel-{key}")))
                    .ghost()
                    .xsmall()
                    .icon(Icon::default().path("icons/x.svg"))
                    .tooltip("Cancel")
                    .on_click(
                        cx.listener(move |this, _: &ClickEvent, _w, cx| this.cancel_rename(cx)),
                    ),
            )
        })
        .when(!editing, |row| row.child(actions))
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, size};
    use gpui_component::Root;
    use scan::{Attribution, PortGroup};
    use std::cell::RefCell;
    use std::rc::Rc;

    fn owned_row(port: u16, pid: u32, how: Attribution) -> PortRow {
        PortRow {
            port,
            pid,
            process: "node".to_string(),
            loopback: true,
            attribution: Some(how),
        }
    }

    fn external_row(port: u16, pid: u32, process: &str) -> PortRow {
        PortRow {
            port,
            pid,
            process: process.to_string(),
            loopback: false,
            attribution: None,
        }
    }

    /// A populated inventory of the shape this machine actually produces: one
    /// claimed project section, and a long external tail.
    fn populated() -> PortInventory {
        PortInventory {
            groups: vec![PortGroup {
                project: PathBuf::from("/work/api"),
                rows: vec![
                    owned_row(3000, 111, Attribution::Terminal),
                    owned_row(5173, 222, Attribution::Cwd),
                ],
            }],
            external: (0..40)
                .map(|i| external_row(5400 + i, 900 + i as u32, "postgres"))
                .collect(),
        }
    }

    /// Mount a panel inside a `Root` and paint it. `Root` is required —
    /// `InputState` panics in `Root::update` without one, and this panel owns
    /// an `InputState` for the rename field whether or not a rename is active.
    fn mount(
        cx: &mut TestAppContext,
        inventory: PortInventory,
    ) -> (gpui::Entity<PortsPanel>, gpui::VisualTestContext) {
        // gpui-component reads a theme global that nothing in a bare test app
        // installs; `Input` and `Button` both panic without it.
        cx.update(gpui_component::init);
        let sink: Rc<RefCell<Option<gpui::Entity<PortsPanel>>>> = Rc::new(RefCell::new(None));
        let out = sink.clone();
        let w = cx.add_window(move |window, cx| {
            let panel = cx.new(|cx| {
                PortsPanel::new(
                    // The panel has to survive a root that is gone: a window
                    // mid-teardown is exactly that, and so is this test.
                    gpui::WeakEntity::new_invalid(),
                    None,
                    Theme::default(),
                    Density::default(),
                    Typography::default(),
                    window,
                    cx,
                )
            });
            *out.borrow_mut() = Some(panel.clone());
            let view: gpui::AnyView = panel.into();
            Root::new(view, window, cx)
        });
        let panel = sink.borrow().clone().expect("the panel was built");
        let mut vcx = gpui::VisualTestContext::from_window(w.into(), cx);
        panel.update(&mut vcx, |panel, cx| panel.apply(inventory, cx));
        // A sidebar's width, not a window's — the panel has to lay out in the
        // narrow column it actually lives in.
        vcx.simulate_resize(size(px(300.0), px(800.0)));
        vcx.run_until_parked();
        (panel, vcx)
    }

    #[gpui::test]
    fn a_populated_panel_paints(cx: &mut TestAppContext) {
        let (panel, vcx) = mount(cx, populated());
        assert_eq!(panel.read_with(&vcx, |p, _| p.count()), 2);
    }

    #[gpui::test]
    fn an_empty_panel_paints(cx: &mut TestAppContext) {
        let (panel, vcx) = mount(cx, PortInventory::default());
        assert_eq!(panel.read_with(&vcx, |p, _| p.count()), 0);
    }

    /// The external section starts closed and its rows are not built until it
    /// is opened — the whole reason it is collapsible is that it is long.
    #[gpui::test]
    fn the_external_section_starts_collapsed_and_opens(cx: &mut TestAppContext) {
        let (panel, mut vcx) = mount(cx, populated());
        assert!(panel.read_with(&vcx, |p, _| p
            .collapsed
            .contains(EXTERNAL_SECTION_KEY)));
        panel.update(&mut vcx, |p, cx| {
            p.toggle_section(EXTERNAL_SECTION_KEY.to_string(), cx)
        });
        vcx.run_until_parked();
        assert!(!panel.read_with(&vcx, |p, _| p
            .collapsed
            .contains(EXTERNAL_SECTION_KEY)));
    }

    /// A project section, unlike the external one, starts open: it is what the
    /// panel was opened for.
    #[gpui::test]
    fn a_project_section_starts_open(cx: &mut TestAppContext) {
        let (panel, vcx) = mount(cx, populated());
        let key = project_section_key(&PathBuf::from("/work/api"));
        assert!(!panel.read_with(&vcx, |p, _| p.collapsed.contains(&key)));
    }

    /// Renaming swaps a row's headline for a live text field. This is the path
    /// that paints an `InputState` inside a row, which is where a layout that
    /// starves the field shows up.
    #[gpui::test]
    fn a_row_being_renamed_still_paints(cx: &mut TestAppContext) {
        let (panel, mut vcx) = mount(cx, populated());
        vcx.update(|window, cx| {
            panel.update(cx, |p, cx| {
                p.begin_rename(PathBuf::from("/work/api"), 3000, window, cx)
            })
        });
        vcx.run_until_parked();
        assert_eq!(
            panel.read_with(&vcx, |p, _| p.rename.clone()),
            Some(RenameTarget {
                project: PathBuf::from("/work/api"),
                port: 3000
            })
        );
    }

    /// An edit whose row vanished between polls has nowhere to commit to, so
    /// it is dropped rather than left pointing at a gone server.
    #[gpui::test]
    fn a_rename_of_a_vanished_row_is_abandoned(cx: &mut TestAppContext) {
        let (panel, mut vcx) = mount(cx, populated());
        vcx.update(|window, cx| {
            panel.update(cx, |p, cx| {
                p.begin_rename(PathBuf::from("/work/api"), 3000, window, cx)
            })
        });
        panel.update(&mut vcx, |p, cx| p.apply(PortInventory::default(), cx));
        vcx.run_until_parked();
        assert_eq!(panel.read_with(&vcx, |p, _| p.rename.clone()), None);
    }

    /// A refresh whose root has gone must not leave the button disabled
    /// forever waiting for a scan nobody is running.
    #[gpui::test]
    fn a_refresh_with_no_root_does_not_wedge_the_button(cx: &mut TestAppContext) {
        let (panel, mut vcx) = mount(cx, populated());
        panel.update(&mut vcx, |p, cx| p.refresh(cx));
        assert!(!panel.read_with(&vcx, |p, _| p.refreshing));
    }
}
