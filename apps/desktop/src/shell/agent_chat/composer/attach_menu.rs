//! The composer's attachment menu: the paperclip at the far left of the toolbar
//! and the rows behind it.
//!
//! Lifted out of `composer.rs` so that file can ratchet back under the size cap.
//! The methods stay inherent on [`ComposerView`] — an inherent impl may live in
//! any module of the crate, and a child module sees its parent's private items,
//! so the menu still reads and writes the composer's own state directly.
//!
//! The menu does no routing of its own. A picked path is handed up as
//! [`ComposerEvent::PathsPicked`] and the issue picker is *asked for* via
//! [`ComposerEvent::OpenForgePicker`], because both need the chat cwd, which
//! lives on the parent view rather than here.

use super::*;

use crate::shell::forge::ForgeKind;

/// Extensions offered by the attach menu's "Add image" picker. A superset of
/// what the wire accepts — the `image_attach` module transcodes the rest — so a
/// bmp or tiff on disk is pickable rather than greyed out.
const IMAGE_PICKER_EXTENSIONS: [&str; 8] =
    ["png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff"];

/// Which native dialog an attach-menu row opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickKind {
    /// Files, filtered to [`IMAGE_PICKER_EXTENSIONS`].
    Images,
    /// Files, unfiltered — an image picked here still attaches as an image,
    /// because the parent routes on what the file IS, not on which row opened
    /// the dialog.
    Files,
    /// Directories.
    Folders,
}

/// What one attachment-menu row does when clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachAction {
    /// Open a native picker.
    Pick(PickKind),
    /// Stage an image sitting on the clipboard.
    PasteImage,
    /// Ask the parent for its issue / pull-request picker.
    OpenForgePicker,
}

/// The attach menu's forge row label. A GitLab repo has merge requests, so
/// naming them "pull requests" would send the user looking for a thing its host
/// does not have.
pub(super) fn forge_attach_label(kind: ForgeKind) -> &'static str {
    match kind {
        ForgeKind::Github => "Add issue or pull request…",
        ForgeKind::Gitlab => "Add issue or merge request…",
    }
}

/// Brand glyph for that row, matching the detected host — the same rule the
/// Create-PR button follows.
fn forge_attach_icon(kind: ForgeKind) -> Icon {
    match kind {
        ForgeKind::Github => gpui_component::IconName::Github.into(),
        ForgeKind::Gitlab => Icon::default().path("icons/gitlab.svg"),
    }
}

impl ComposerView {
    /// Mirror the parent's forge detection so the attach menu's issue row can
    /// name the right thing (a GitLab repo has merge requests, not pull
    /// requests) and hide itself on a repo no forge claims. Only repaints on a
    /// real change — detection lands once per chat.
    pub fn set_forge_kind(&mut self, kind: Option<ForgeKind>, cx: &mut Context<Self>) {
        if self.forge_kind != kind {
            self.forge_kind = kind;
            cx.notify();
        }
    }

    /// Which forge backs this chat's repo, if the parent has detected one. Read
    /// by the parent to decide whether the picker has anything to list.
    pub fn forge_kind(&self) -> Option<ForgeKind> {
        self.forge_kind
    }

    /// Open the native picker for `kind` and hand whatever the user chose up to
    /// the parent as [`ComposerEvent::PathsPicked`].
    ///
    /// `rfd`'s async dialog runs off the main thread, so this never blocks the
    /// window. Nothing is read or decoded here: the parent decides what each
    /// path becomes, which is what keeps a picked file and a dropped file on one
    /// code path.
    fn pick_paths(&mut self, kind: PickKind, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let dialog = rfd::AsyncFileDialog::new();
            let picked = match kind {
                PickKind::Images => {
                    dialog
                        .add_filter("Images", &IMAGE_PICKER_EXTENSIONS)
                        .pick_files()
                        .await
                }
                PickKind::Files => dialog.pick_files().await,
                PickKind::Folders => dialog.pick_folders().await,
            };
            let Some(picked) = picked else { return };
            let paths: Vec<std::path::PathBuf> =
                picked.into_iter().map(|f| f.path().to_path_buf()).collect();
            if paths.is_empty() {
                return;
            }
            let _ = this.update(cx, |_this, cx| cx.emit(ComposerEvent::PathsPicked(paths)));
        })
        .detach();
    }

    /// The attach control (far left of the toolbar): a flat ghost paperclip that
    /// opens the attachment menu. Always enabled — attachments stage for the
    /// next send even while a turn streams.
    ///
    /// A menu rather than the single image picker it used to be: images were
    /// only ever one of the things a user attaches here, and everything else
    /// (a file, a folder, an issue) was reachable only by dragging it in or
    /// knowing to type `@`. What each row does is unchanged plumbing — the rows
    /// make it findable.
    pub(super) fn render_attach_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let trigger = Button::new("chat-attach-btn")
            .icon(Icon::default().path("icons/paperclip.svg"))
            .ghost()
            .small();
        let forge_kind = self.forge_kind;

        let build_menu = move |mut menu: PopupMenu,
                               window: &mut Window,
                               _cx: &mut Context<PopupMenu>| {
            // Each row is `(label, icon, what it does)`. Built as a list so the
            // click plumbing is written once — the rows differ only in which
            // method they call.
            let mut rows: Vec<(&'static str, Icon, AttachAction)> = vec![
                ("Add image…", Icon::default().path("icons/image.svg"), AttachAction::Pick(PickKind::Images)),
                (
                    "Paste image",
                    Icon::default().path("icons/clipboard-paste.svg"),
                    AttachAction::PasteImage,
                ),
            ];
            // Hidden on a repo no forge claims: the picker there could only ever
            // come back empty, and an always-visible row that never works is
            // worse than one that isn't offered.
            if let Some(kind) = forge_kind {
                rows.push((forge_attach_label(kind), forge_attach_icon(kind), AttachAction::OpenForgePicker));
            }
            rows.push((
                "Add files…",
                Icon::default().path("icons/paperclip.svg"),
                AttachAction::Pick(PickKind::Files),
            ));
            rows.push((
                "Add folder…",
                Icon::default().path("icons/folder-plus.svg"),
                AttachAction::Pick(PickKind::Folders),
            ));

            for (label, icon, action) in rows {
                let view = view.clone();
                menu = menu.item(
                    PopupMenuItem::new(label).icon(icon).on_click(window.listener_for(
                        &view,
                        move |v: &mut ComposerView, _e: &gpui::ClickEvent, _w, cx| {
                            v.run_attach_action(action, cx)
                        },
                    )),
                );
            }
            menu
        };

        self.render_dropdown_shell(
            "chat-attach".into(),
            "Attach".into(),
            trigger,
            // Leftmost control in the toolbar, so the menu opens up and to the
            // RIGHT — the same placement the mic menu uses next to it.
            Anchor::BottomLeft,
            build_menu,
            cx,
        )
    }

    /// Run one attachment-menu row. Split out so every row shares the same click
    /// plumbing in [`Self::render_attach_button`].
    fn run_attach_action(&mut self, action: AttachAction, cx: &mut Context<Self>) {
        match action {
            AttachAction::Pick(kind) => self.pick_paths(kind, cx),
            AttachAction::PasteImage => {
                // Only the menu row reports the miss. The ⌘V path deliberately
                // stays silent and falls through to a text paste, which is what
                // a keystroke on a text clipboard should do; picking this row is
                // an explicit ask for an image, so "there isn't one" is an
                // answer the user needs.
                if !self.try_paste_image(cx) {
                    self.pending_toast = Some("No image on the clipboard".into());
                    cx.notify();
                }
            }
            AttachAction::OpenForgePicker => cx.emit(ComposerEvent::OpenForgePicker),
        }
    }
}
