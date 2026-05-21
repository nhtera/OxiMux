//! Add-Project dialog — three-card chooser for adding a project.
//!
//! Reached from the left rail toolbar's "Add Project" button and from
//! the project picker's "Open Folder…" row. Three cards:
//!
//! - **Browse folder** — wired: `rfd::AsyncFileDialog::pick_folder()` →
//!   `ProjectRepo::insert_or_touch` → `OnPick` callback, identical to
//!   the existing picker affordance.
//! - **Clone from URL** — disabled stub for v1. Visible to set
//!   expectations; tooltip "Coming soon".
//! - **Remote project** — disabled stub for v1. Same treatment.
//!
//! Layout mirrors `project_picker.rs` and `workspace_dialog.rs`: full-
//! window overlay for click-outside dismiss, centered card with
//! `FocusHandle` for keyboard escape.

use std::path::{Path, PathBuf};

use gpui::{
    App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, ParentElement, Render, Styled, Window, div, px,
};
use oximux_core::Project;
use oximux_settings::{Density, Theme, Typography};
use oximux_storage::ProjectRepo;

/// Card grid width.
const MODAL_WIDTH: f32 = 560.0;
/// Vertical offset from the top of the viewport.
const MODAL_TOP_OFFSET: f32 = 120.0;
/// Single card height.
const CARD_HEIGHT: f32 = 120.0;
/// Fallback name when `path.file_name()` returns None.
const FALLBACK_PROJECT_NAME: &str = "untitled";
/// Branch stored at insert time; real HEAD detection lands in step 9.
const DEFAULT_BRANCH_PLACEHOLDER: &str = "main";

/// Owner callback fired after a project is added via Browse-folder.
pub type OnPick = Box<dyn Fn(Project, &mut Window, &mut App) + Send + 'static>;

/// The three cards. Only `BrowseFolder` is wired in v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CardKind {
    BrowseFolder,
    CloneFromUrl,
    RemoteProject,
}

impl CardKind {
    fn title(self) -> &'static str {
        match self {
            Self::BrowseFolder => "Browse folder",
            Self::CloneFromUrl => "Clone from URL",
            Self::RemoteProject => "Remote project",
        }
    }

    fn subtitle(self) -> &'static str {
        match self {
            Self::BrowseFolder => "Local Git project or folder",
            Self::CloneFromUrl => "Remote Git repository",
            Self::RemoteProject => "SSH connected target",
        }
    }

    fn enabled(self) -> bool {
        matches!(self, Self::BrowseFolder)
    }
}

pub struct AddProjectDialog {
    open: bool,
    pending_folder_pick: bool,
    focus_handle: FocusHandle,
    project_repo: ProjectRepo,
    on_pick: OnPick,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl AddProjectDialog {
    pub fn new(
        theme: Theme,
        density: Density,
        typography: Typography,
        project_repo: ProjectRepo,
        on_pick: OnPick,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            open: false,
            pending_folder_pick: false,
            focus_handle: cx.focus_handle(),
            project_repo,
            on_pick,
            theme,
            density,
            typography,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        tracing::info!("AddProjectDialog: open()");
        self.open = true;
        self.pending_folder_pick = false;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.pending_folder_pick = false;
        cx.notify();
    }

    fn trigger_browse(&mut self, cx: &mut Context<Self>) {
        tracing::info!("AddProjectDialog: Browse folder clicked");
        if self.pending_folder_pick {
            tracing::info!("AddProjectDialog: pending_folder_pick already set; ignoring");
            return;
        }
        self.pending_folder_pick = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            tracing::info!("AddProjectDialog: opening NSOpenPanel");
            let folder = rfd::AsyncFileDialog::new().pick_folder().await;
            let path = folder.map(|h| h.path().to_path_buf());
            tracing::info!(?path, "AddProjectDialog: NSOpenPanel resolved");
            let _ = this.update_in(cx, |this, window, cx| match path {
                Some(p) => this.handle_folder_pick(p, window, cx),
                None => {
                    this.pending_folder_pick = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn handle_folder_pick(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        tracing::info!(path = %path.display(), "AddProjectDialog: handle_folder_pick");
        // NSOpenPanel races against close(); discard stale results.
        if !self.open {
            tracing::warn!("AddProjectDialog: handle_folder_pick on a closed dialog; dropping");
            return;
        }
        self.pending_folder_pick = false;
        let path_str = path.to_string_lossy().to_string();
        let name = name_from_path(&path);
        match self
            .project_repo
            .insert_or_touch(&name, &path_str, DEFAULT_BRANCH_PLACEHOLDER)
        {
            Ok(project) => {
                tracing::info!(project_id = %project.id, "AddProjectDialog: invoking on_pick");
                // Close before invoking the callback so any modal the
                // callback opens isn't wiped by a trailing close.
                self.close(cx);
                (self.on_pick)(project, window, cx);
            }
            Err(err) => {
                tracing::warn!(?err, path = %path_str, "insert_or_touch failed");
                cx.notify();
            }
        }
    }
}

impl Focusable for AddProjectDialog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AddProjectDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();

        let title = div()
            .text_size(px(typography.t_body_md * 1.15))
            .font_weight(typography.w_semibold)
            .text_color(theme.fg_base)
            .child("Add a project");

        let subtitle = div()
            .text_size(px(typography.t_body_sm))
            .text_color(theme.fg_muted)
            .child("Add another project to manage with OxiMux.");

        let card_row = div()
            .flex()
            .flex_row()
            .gap(px(density.gap_inline * 1.5))
            .w_full()
            .child(self.render_card(CardKind::BrowseFolder, cx))
            .child(self.render_card(CardKind::CloneFromUrl, cx))
            .child(self.render_card(CardKind::RemoteProject, cx));

        let footer_link = div()
            .text_size(px(typography.t_body_sm))
            .text_color(theme.fg_subtle)
            .child("Or start a new project from scratch");

        let card = div()
            .flex()
            .flex_col()
            .gap(px(density.gap_inline * 1.5))
            .w(px(MODAL_WIDTH))
            .p(px(density.pad_panel * 2.0))
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(theme.border_active)
            .rounded(px(density.r_card))
            .shadow_lg()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _window, cx| {
                if ev.keystroke.key == "escape" {
                    this.close(cx);
                }
            }))
            .child(title)
            .child(subtitle)
            .child(card_row)
            .child(div().flex().justify_center().child(footer_link));

        div()
            .absolute()
            .inset_0()
            .size_full()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _window, cx| this.close(cx)),
            )
            .child(
                div()
                    .absolute()
                    .top(px(MODAL_TOP_OFFSET))
                    .flex()
                    .w_full()
                    .justify_center()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(card),
            )
            .into_any_element()
    }
}

impl AddProjectDialog {
    fn render_card(&self, kind: CardKind, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let enabled = kind.enabled();
        let (bg, fg, sub_fg) = if enabled {
            (theme.bg_panel, theme.fg_base, theme.fg_muted)
        } else {
            (theme.bg_panel_alt, theme.fg_subtle, theme.fg_subtle)
        };

        let mut card = div()
            .id(("add-project-card", kind as usize))
            .flex()
            .flex_1()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(density.gap_inline))
            .h(px(CARD_HEIGHT))
            .px(px(density.pad_panel))
            .bg(bg)
            .border_1()
            .border_color(theme.border_inactive)
            .rounded(px(density.r_card))
            .child(
                div()
                    .text_size(px(typography.t_body_md))
                    .font_weight(typography.w_semibold)
                    .text_color(fg)
                    .child(kind.title()),
            )
            .child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .text_color(sub_fg)
                    .child(kind.subtitle()),
            );

        if enabled {
            card = card
                .cursor_pointer()
                .hover(|s| s.bg(theme.bg_panel_alt))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                        if kind == CardKind::BrowseFolder {
                            this.trigger_browse(cx);
                        }
                    }),
                );
        } else {
            // Disabled — show a "Coming soon" footnote in place of hover.
            card = card.child(
                div()
                    .text_size(px(typography.t_body_sm * 0.85))
                    .text_color(theme.fg_subtle)
                    .child("Coming soon"),
            );
        }
        card
    }
}

fn name_from_path(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| FALLBACK_PROJECT_NAME.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browse_card_is_enabled() {
        assert!(CardKind::BrowseFolder.enabled());
    }

    #[test]
    fn clone_card_is_disabled() {
        assert!(!CardKind::CloneFromUrl.enabled());
    }

    #[test]
    fn remote_card_is_disabled() {
        assert!(!CardKind::RemoteProject.enabled());
    }

    #[test]
    fn name_from_path_uses_basename() {
        assert_eq!(
            name_from_path(&PathBuf::from("/Users/a/Code/oximux")),
            "oximux"
        );
    }

    #[test]
    fn name_from_path_fallback_on_root() {
        assert_eq!(name_from_path(&PathBuf::from("/")), FALLBACK_PROJECT_NAME);
    }

    #[test]
    fn name_from_path_strips_trailing_slash() {
        assert_eq!(name_from_path(&PathBuf::from("/tmp/foo/")), "foo");
    }
}
