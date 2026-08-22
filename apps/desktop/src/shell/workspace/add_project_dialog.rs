//! Add-Project dialog — card chooser plus an inline Clone-from-URL form.
//!
//! Reached from the left rail toolbar's "Add Project" button and from
//! the project picker's "Open Folder…" row. Two cards:
//!
//! - **Browse folder** — `rfd::AsyncFileDialog::pick_folder()` →
//!   `ProjectRepo::insert_or_touch` → `OnPick` callback, identical to
//!   the existing picker affordance.
//! - **Clone from URL** — switches the dialog to a clone form: a URL
//!   field, then a destination-folder pick → `git clone` (off the GPUI
//!   thread, on the tokio runtime) → register + open as a project.
//!
//! Layout mirrors `project_picker.rs` and `workspace_dialog.rs`: full-
//! window overlay for click-outside dismiss, centered card with
//! `FocusHandle` for keyboard escape.

use std::path::{Path, PathBuf};

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Render, Styled, Window,
    div, px,
};
use gpui_component::{
    Disableable,
    button::{Button, ButtonVariants},
    input::{Input, InputState},
};
use oximux_core::Project;
use oximux_settings::{Density, Theme, Typography};

use crate::ui::FloatingSurface;
use oximux_storage::ProjectRepo;
use tokio::sync::oneshot;

/// Card grid width.
const MODAL_WIDTH: f32 = 560.0;
/// Vertical offset from the top of the viewport.
const MODAL_TOP_OFFSET: f32 = 120.0;
/// Single card height.
const CARD_HEIGHT: f32 = 120.0;
/// Fallback name when `path.file_name()` returns None.
const FALLBACK_PROJECT_NAME: &str = "untitled";
/// Branch stored at insert time; real HEAD detection lands later.
pub(crate) const DEFAULT_BRANCH_PLACEHOLDER: &str = "main";

/// Owner callback fired after a project is added (Browse or Clone).
pub type OnPick = Box<dyn Fn(Project, &mut Window, &mut App) + Send + 'static>;

/// Which surface the open dialog is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DialogView {
    /// The card chooser (Browse / Clone).
    Cards,
    /// The Clone-from-URL form.
    Clone,
}

/// The chooser cards. Both are wired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CardKind {
    BrowseFolder,
    CloneFromUrl,
}

impl CardKind {
    fn title(self) -> &'static str {
        match self {
            Self::BrowseFolder => "Browse folder",
            Self::CloneFromUrl => "Clone from URL",
        }
    }

    fn subtitle(self) -> &'static str {
        match self {
            Self::BrowseFolder => "Local Git project or folder",
            Self::CloneFromUrl => "Remote Git repository",
        }
    }
}

pub struct AddProjectDialog {
    open: bool,
    view: DialogView,
    pending_folder_pick: bool,
    /// True while a `git clone` is in flight (picker + network).
    cloning: bool,
    /// Last clone failure, shown inline under the URL field.
    clone_error: Option<String>,
    /// URL field for the clone form. Created lazily on first `open` (the
    /// constructor runs without a `&mut Window`, which `InputState` needs).
    url_input: Option<Entity<InputState>>,
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
            view: DialogView::Cards,
            pending_folder_pick: false,
            cloning: false,
            clone_error: None,
            url_input: None,
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
        self.open = true;
        self.view = DialogView::Cards;
        self.pending_folder_pick = false;
        self.cloning = false;
        self.clone_error = None;
        match &self.url_input {
            Some(input) => input.update(cx, |s, cx| s.set_value("", window, cx)),
            None => {
                let input = cx.new(|cx| {
                    InputState::new(window, cx).placeholder("https://github.com/owner/repo.git")
                });
                self.url_input = Some(input);
            }
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.view = DialogView::Cards;
        self.pending_folder_pick = false;
        self.cloning = false;
        self.clone_error = None;
        cx.notify();
    }

    /// Switch to the clone form (or back to the cards). Clears any stale
    /// error so the form opens clean.
    fn set_view(&mut self, view: DialogView, cx: &mut Context<Self>) {
        self.view = view;
        self.clone_error = None;
        cx.notify();
    }

    fn trigger_browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_folder_pick {
            return;
        }
        self.pending_folder_pick = true;
        cx.notify();
        // Root the spawn in the window (`spawn_in` + `update_in`): a bare
        // `cx.spawn` + `update_in` silently drops its body when rfd's
        // NSOpenPanel resolves outside the GPUI window context, leaving the
        // dialog frozen with `pending_folder_pick` stuck true.
        cx.spawn_in(window, async move |this, cx| {
            let folder = rfd::AsyncFileDialog::new().pick_folder().await;
            let path = folder.map(|h| h.path().to_path_buf());
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
        // NSOpenPanel races against close(); discard stale results.
        if !self.open {
            return;
        }
        self.pending_folder_pick = false;
        self.register_and_open(path, window, cx);
    }

    /// Kick off a clone: validate the URL, derive a folder name, pick a
    /// destination parent, then run `git clone` on the tokio runtime and
    /// register the result. Single-flight on `cloning`.
    fn trigger_clone(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.cloning {
            return;
        }
        let Some(input) = self.url_input.clone() else {
            return;
        };
        let url = input.read(cx).value().trim().to_string();
        if url.is_empty() {
            self.clone_error = Some("Enter a repository URL.".to_string());
            cx.notify();
            return;
        }
        let Some(name) = oximux_git::repo_name_from_url(&url) else {
            self.clone_error = Some("Could not read a repository name from that URL.".to_string());
            cx.notify();
            return;
        };
        // Git I/O must run on the tokio runtime, not the GPUI executor.
        let handle = match tokio::runtime::Handle::try_current() {
            Ok(h) => h,
            Err(_) => {
                self.clone_error = Some("No runtime available to clone.".to_string());
                cx.notify();
                return;
            }
        };
        self.cloning = true;
        self.clone_error = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let parent = rfd::AsyncFileDialog::new()
                .pick_folder()
                .await
                .map(|h| h.path().to_path_buf());
            let Some(parent) = parent else {
                // User cancelled the folder pick — drop back to the form.
                let _ = this.update(cx, |this, cx| {
                    this.cloning = false;
                    cx.notify();
                });
                return;
            };
            let dest = parent.join(&name);
            let (tx, rx) = oneshot::channel::<Result<PathBuf, String>>();
            handle.spawn(async move {
                let _ = tx.send(
                    oximux_git::clone_repo(&url, &dest)
                        .await
                        .map_err(|e| e.to_string()),
                );
            });
            let outcome = rx.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.cloning = false;
                match outcome {
                    Ok(Ok(path)) => this.register_and_open(path, window, cx),
                    Ok(Err(msg)) => {
                        this.clone_error = Some(msg);
                        cx.notify();
                    }
                    Err(_) => {
                        this.clone_error = Some("Clone was cancelled.".to_string());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// Register `path` as a project and hand it to the owner callback.
    /// Shared by Browse and Clone. Closes the dialog before firing
    /// `on_pick` so a modal the callback opens isn't wiped by a trailing
    /// close.
    fn register_and_open(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        let path_str = path.to_string_lossy().to_string();
        let name = name_from_path(&path);
        match self
            .project_repo
            .insert_or_touch(&name, &path_str, DEFAULT_BRANCH_PLACEHOLDER)
        {
            Ok(project) => {
                self.close(cx);
                (self.on_pick)(project, window, cx);
            }
            Err(err) => {
                tracing::warn!(?err, path = %path_str, "insert_or_touch failed");
                self.clone_error = Some("Failed to register the project.".to_string());
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
        oximux_settings::appearance::sync(&mut self.density, &mut self.typography, cx);
        if !self.open {
            return div().into_any_element();
        }
        let theme = self.theme;
        let density = self.density;

        let body = match self.view {
            DialogView::Cards => self.render_cards(cx).into_any_element(),
            DialogView::Clone => self.render_clone_form(cx).into_any_element(),
        };

        let card = div()
            .flex()
            .flex_col()
            .gap(px(density.gap_inline * 1.5))
            .w(px(MODAL_WIDTH))
            .p(px(density.pad_panel * 2.0))
            .floating_chrome(&theme, &density)
            .shadow_lg()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                match ev.keystroke.key.as_str() {
                    "escape" => match this.view {
                        DialogView::Clone => this.set_view(DialogView::Cards, cx),
                        DialogView::Cards => this.close(cx),
                    },
                    "enter" if matches!(this.view, DialogView::Clone) => {
                        this.trigger_clone(window, cx)
                    }
                    _ => {}
                }
            }))
            .child(body);

        div()
            .absolute()
            .inset_0()
            .size_full()
            .occlude()
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
    fn render_cards(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
            .child(self.render_card(CardKind::CloneFromUrl, cx));

        let footer_link = div()
            .text_size(px(typography.t_body_sm))
            .text_color(theme.fg_subtle)
            .child("Or start a new project from scratch");

        div()
            .flex()
            .flex_col()
            .gap(px(density.gap_inline * 1.5))
            .child(title)
            .child(subtitle)
            .child(card_row)
            .child(div().flex().justify_center().child(footer_link))
    }

    fn render_card(&self, kind: CardKind, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();

        div()
            .id(("add-project-card", kind as usize))
            .flex()
            .flex_1()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(density.gap_inline))
            .h(px(CARD_HEIGHT))
            .px(px(density.pad_panel))
            .bg(theme.bg_panel)
            .border_1()
            .border_color(theme.border_inactive)
            .rounded(px(density.r_card))
            .cursor_pointer()
            .hover(|s| s.bg(theme.hover_overlay))
            .child(
                div()
                    .text_size(px(typography.t_body_md))
                    .font_weight(typography.w_semibold)
                    .text_color(theme.fg_base)
                    .child(kind.title()),
            )
            .child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_muted)
                    .child(kind.subtitle()),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| match kind {
                    CardKind::BrowseFolder => this.trigger_browse(window, cx),
                    CardKind::CloneFromUrl => this.set_view(DialogView::Clone, cx),
                }),
            )
    }

    fn render_clone_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let cloning = self.cloning;

        let title = div()
            .text_size(px(typography.t_body_md * 1.15))
            .font_weight(typography.w_semibold)
            .text_color(theme.fg_base)
            .child("Clone from URL");

        let label = div()
            .text_size(px(typography.t_label_caps))
            .text_color(theme.fg_subtle)
            .child("Repository URL");

        let mut col = div()
            .flex()
            .flex_col()
            .gap(px(density.gap_inline))
            .child(title)
            .child(label);

        if let Some(input) = &self.url_input {
            col = col.child(Input::new(input).disabled(cloning));
        }

        if cloning {
            col = col.child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_muted)
                    .child("Cloning… choose a destination folder when prompted."),
            );
        } else if let Some(err) = &self.clone_error {
            col = col.child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.status_error)
                    .child(err.clone()),
            );
        } else {
            col = col.child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_subtle)
                    .child("You'll pick a destination folder next."),
            );
        }

        col.child(
            div()
                .flex()
                .flex_row()
                .justify_end()
                .gap(px(density.gap_inline))
                .child(
                    Button::new("add-project-clone-back")
                        .label("Back")
                        .disabled(cloning)
                        .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                            this.set_view(DialogView::Cards, cx);
                        })),
                )
                .child(
                    Button::new("add-project-clone-submit")
                        .primary()
                        .label("Clone")
                        .disabled(cloning)
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.trigger_clone(window, cx);
                        })),
                ),
        )
    }
}

pub(crate) fn name_from_path(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| FALLBACK_PROJECT_NAME.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_titles_resolve() {
        assert_eq!(CardKind::BrowseFolder.title(), "Browse folder");
        assert_eq!(CardKind::CloneFromUrl.title(), "Clone from URL");
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
