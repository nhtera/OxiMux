//! Ports panel — what the terminals in this window are currently serving.
//!
//! **Why this is a panel and not a log line.** A dev server prints its URL
//! once, at the moment it starts, and then a build log buries it. Ten minutes
//! later "was that 5173 or 5174?" costs a scroll-back, and after a restart on a
//! different port it costs a wrong answer. The socket outlives the line: it
//! exists for exactly as long as the server is accepting connections, so a
//! panel reading the socket table is always current in a way a transcript
//! never is.
//!
//! **Why it is scoped to your terminals.** The kernel would happily list every
//! listener on the machine, and that list is mostly the OS talking to itself.
//! Attribution runs the other way — from the terminals this window owns, down
//! their process trees, and only then to the socket table — so every row here
//! is something the user started, under the project they started it in. See
//! [`oximux_proc_ports`] for why that scoping is also what keeps the Linux
//! implementation affordable.
//!
//! **Why labels are persisted.** Three `node` rows on 3000, 3001 and 9229 are
//! the API, the docs site and a debugger, and nothing the kernel knows can
//! tell them apart. The name is the user's to write, so it is stored against
//! project+port and comes back when the same server does.
//!
//! The scan itself is driven by [`crate::workspace_root::WorkspaceRoot`] —
//! it owns the terminals the walk starts from, and the socket read belongs on
//! a background thread. This module renders what the scan produced and owns
//! the actions on a row.

pub(crate) mod labels;
pub(crate) mod scan;

use std::collections::HashMap;
use std::path::PathBuf;

use gpui::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled, WeakEntity, Window, div, prelude::FluentBuilder as _,
    px,
};
use gpui_component::Sizable as _;
use gpui_component::input::{Input, InputEvent, InputState};
use oximux_settings::{Density, Theme, Typography};
use oximux_storage::SettingsRepo;

use crate::app_settings::port_label_settings;
use crate::shell::settings_modal::controls::value_chip;
use crate::workspace_root::WorkspaceRoot;

use labels::{
    empty_detail, empty_headline, origin_label, port_metric_label, project_label, reach_label,
    row_title, url_for,
};
use scan::PortInventory;

/// Which row's label is being edited. Project *and* port, because that pair is
/// the label's identity — a pid would be gone by the next poll and the row
/// would stop matching mid-edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RenameTarget {
    pub project: PathBuf,
    pub port: u16,
}

pub struct PortsPanel {
    weak_root: WeakEntity<WorkspaceRoot>,
    focus_handle: FocusHandle,
    /// `None` in tests; a panel without a store simply never persists.
    settings_repo: Option<SettingsRepo>,

    inventory: PortInventory,
    /// Whether the window has any terminal at all. Distinguishes "nothing is
    /// serving" from "there is nothing to look inside", which need different
    /// empty states — see [`empty_headline`].
    has_terminals: bool,
    /// Persisted labels by [`port_label_settings::label_key`]. Loaded once and
    /// updated in place on write, so a poll never touches the database.
    port_labels: HashMap<String, String>,

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
        let rename_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Name this port"));
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
            has_terminals: false,
            port_labels,
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
    /// forever is a battery bug wearing a feature's clothes.
    pub fn apply(&mut self, inventory: PortInventory, has_terminals: bool, cx: &mut Context<Self>) {
        if self.inventory == inventory && self.has_terminals == has_terminals {
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
        self.has_terminals = has_terminals;
        cx.notify();
    }

    /// Total rows, for the status bar's metric.
    pub fn count(&self) -> usize {
        self.inventory.total()
    }

    fn label_for(&self, project: &std::path::Path, port: u16) -> Option<&str> {
        self.port_labels
            .get(&port_label_settings::label_key(project, port))
            .map(String::as_str)
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
        let _ = self.weak_root.update(cx, |root, cx| {
            root.push_toast(
                crate::shell::toast::ToastKind::Info,
                format!("Copied {}", url_for(port)),
                cx,
            );
        });
    }

    /// Ask the root for an out-of-cadence scan.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let _ = self.weak_root.update(cx, |root, cx| {
            root.run_port_scan(cx);
        });
    }
}

impl Focusable for PortsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PortsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.density, &mut self.typography, cx);
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
                .gap(px(density.gap_inline))
                .p(px(density.pad_panel))
                .overflow_y_scroll()
                .track_scroll(&self.scroll);
            for group in 0..self.inventory.groups.len() {
                col = col.child(self.render_group(group, cx));
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
        let density = self.density;
        let typography = self.typography.clone();
        let count = self.count();

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .flex_none()
            .h(px(density.h_top_bar))
            .px(px(density.pad_panel))
            .border_b_1()
            .border_color(theme.border_inactive)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_size(px(typography.t_body_md))
                            .font_weight(typography.w_semibold)
                            .text_color(theme.fg_base)
                            .child("Ports"),
                    )
                    .child(
                        div()
                            .text_size(px(typography.t_sub_label))
                            .text_color(theme.fg_subtle)
                            .child(port_metric_label(count)),
                    ),
            )
            .child(value_chip(
                "ports-refresh",
                "Refresh",
                theme,
                density,
                &typography,
                |this: &mut Self, _w, cx| this.refresh(cx),
                cx,
            ))
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
                    .child(empty_headline(self.has_terminals)),
            )
            .child(
                div()
                    .text_size(px(typography.t_sub_label))
                    .text_color(theme.fg_subtle)
                    .text_center()
                    .child(empty_detail(self.has_terminals)),
            )
            .into_any_element()
    }

    fn render_group(&mut self, idx: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let project = self.inventory.groups[idx].project.clone();
        let heading = project_label(&project);
        let row_count = self.inventory.groups[idx].rows.len();

        let mut col = div()
            .flex()
            .flex_col()
            .w_full()
            .gap(px(density.gap_inline))
            .child(
                div()
                    .text_size(px(typography.t_sub_label))
                    .font_weight(typography.w_semibold)
                    .text_color(theme.fg_subtle)
                    .child(heading.to_uppercase()),
            );
        for row in 0..row_count {
            col = col.child(self.render_row(idx, row, &project, cx));
        }
        col.into_any_element()
    }

    fn render_row(
        &mut self,
        group: usize,
        row: usize,
        project: &std::path::Path,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let entry = &self.inventory.groups[group].rows[row];
        let port = entry.port;
        let pid = entry.pid;
        let process = entry.process.clone();
        let loopback = entry.loopback;
        let title = row_title(self.label_for(project, port), &process, port);
        let editing = self.rename.as_ref().is_some_and(|t| {
            t.port == port && t.project == project
        });
        let owned_project = project.to_path_buf();

        // Element ids must be unique across the whole panel, and a port number
        // alone is not: two projects can each serve 3000 only if one of them
        // has exited, but the panel may render both in the frame between.
        let key = format!("{group}-{row}");

        div()
            .flex()
            .flex_col()
            .w_full()
            .gap(px(4.0))
            .p(px(density.pad_row))
            .rounded(px(density.r_card))
            .bg(theme.bg_panel_alt)
            .border_1()
            .border_color(theme.border_inactive)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(density.gap_inline))
                    .w_full()
                    .child(if editing {
                        div()
                            // `flex_1` claims the free space; `min_w_0` lets
                            // the field shrink instead of forcing the row
                            // wider than the sidebar.
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.rename_input).small())
                            .into_any_element()
                    } else {
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(typography.t_body_sm))
                            .font_weight(typography.w_semibold)
                            .text_color(theme.fg_base)
                            .child(title)
                            .into_any_element()
                    })
                    .child(
                        div()
                            .flex_none()
                            .px(px(6.0))
                            .rounded(px(density.r_chip))
                            .bg(theme.bg_panel)
                            .border_1()
                            .border_color(theme.border_inactive)
                            .text_size(px(typography.t_sub_label))
                            .text_color(theme.fg_muted)
                            .child(format!(":{port}")),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(density.gap_inline))
                    .child(
                        div()
                            .text_size(px(typography.t_sub_label))
                            .text_color(theme.fg_subtle)
                            .child(origin_label(&process, pid)),
                    )
                    .child(
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
                            .child(reach_label(loopback)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap(px(density.gap_inline))
                    .when(!editing, |actions| {
                        actions
                            .child(value_chip(
                                SharedString::from(format!("port-open-{key}")),
                                "Open",
                                theme,
                                density,
                                &typography,
                                move |this: &mut Self, _w, cx| this.open_port(port, cx),
                                cx,
                            ))
                            .child(value_chip(
                                SharedString::from(format!("port-copy-{key}")),
                                "Copy URL",
                                theme,
                                density,
                                &typography,
                                move |this: &mut Self, _w, cx| this.copy_url(port, cx),
                                cx,
                            ))
                            .child(value_chip(
                                SharedString::from(format!("port-rename-{key}")),
                                "Rename",
                                theme,
                                density,
                                &typography,
                                move |this: &mut Self, window, cx| {
                                    this.begin_rename(owned_project.clone(), port, window, cx)
                                },
                                cx,
                            ))
                    })
                    .when(editing, |actions| {
                        actions
                            .child(value_chip(
                                SharedString::from(format!("port-save-{key}")),
                                "Save",
                                theme,
                                density,
                                &typography,
                                move |this: &mut Self, window, cx| {
                                    this.commit_rename(window, cx)
                                },
                                cx,
                            ))
                            .child(value_chip(
                                SharedString::from(format!("port-cancel-{key}")),
                                "Cancel",
                                theme,
                                density,
                                &typography,
                                move |this: &mut Self, _w, cx| this.cancel_rename(cx),
                                cx,
                            ))
                    }),
            )
            .into_any_element()
    }
}
