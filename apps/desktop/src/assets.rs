//! Composite asset source: local app SVGs first, then gpui-component's bundle.
//!
//! Lets us ship icons the upstream component crate doesn't bundle (e.g.
//! `git-branch.svg` for the Source Control tab) while still falling through
//! to the rich `IconName::*` catalog for everything else.
//!
//! Wired in `main.rs::with_assets(CompositeAssets)`.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, SharedString};

/// Local SVGs embedded via `include_bytes!`. Add a match arm + corresponding
/// file in `apps/desktop/assets/icons/` to register a new asset.
struct AppAssets;

/// SVGs shipped locally. Each entry is `(asset path, embedded bytes)`. To add
/// a new icon, drop the file under `apps/desktop/assets/icons/` and append a
/// tuple here — no other wiring needed.
const APP_ICONS: &[(&str, &[u8])] = &[
    (
        "icons/git-branch.svg",
        include_bytes!("../assets/icons/git-branch.svg"),
    ),
    // Clock/history glyph for the chat composer's "Sessions" browser button.
    (
        "icons/history.svg",
        include_bytes!("../assets/icons/history.svg"),
    ),
    // Pencil glyph for the "Edit message" action on a user chat bubble.
    (
        "icons/pencil.svg",
        include_bytes!("../assets/icons/pencil.svg"),
    ),
    // Terminal glyph for the new-tab picker's "New Terminal" quick action.
    (
        "icons/square-terminal.svg",
        include_bytes!("../assets/icons/square-terminal.svg"),
    ),
    // Eye glyph for the chat tab's view-switcher menu button.
    ("icons/eye.svg", include_bytes!("../assets/icons/eye.svg")),
    // Locate glyph for the left rail's scroll-to-current-workspace button.
    (
        "icons/crosshair.svg",
        include_bytes!("../assets/icons/crosshair.svg"),
    ),
    // GitLab brand glyph for the Create-PR button — the upstream icon
    // catalog ships only a GitHub mark.
    (
        "icons/gitlab.svg",
        include_bytes!("../assets/icons/gitlab.svg"),
    ),
    (
        "icons/list-collapse.svg",
        include_bytes!("../assets/icons/list-collapse.svg"),
    ),
    // Add-project glyph for the left rail's Projects header.
    (
        "icons/folder-plus.svg",
        include_bytes!("../assets/icons/folder-plus.svg"),
    ),
    // Compact/detailed card-density toggle in the left rail's Projects header.
    (
        "icons/rows-2.svg",
        include_bytes!("../assets/icons/rows-2.svg"),
    ),
    (
        "icons/refresh-cw.svg",
        include_bytes!("../assets/icons/refresh-cw.svg"),
    ),
    // "New chat" glyph for the agent-chat composer toolbar.
    (
        "icons/plus.svg",
        include_bytes!("../assets/icons/plus.svg"),
    ),
    (
        "icons/circle-slash.svg",
        include_bytes!("../assets/icons/circle-slash.svg"),
    ),
    (
        "icons/file-text.svg",
        include_bytes!("../assets/icons/file-text.svg"),
    ),
    (
        "icons/file-box.svg",
        include_bytes!("../assets/icons/file-box.svg"),
    ),
    (
        "icons/file-cog.svg",
        include_bytes!("../assets/icons/file-cog.svg"),
    ),
    (
        "icons/file-code.svg",
        include_bytes!("../assets/icons/file-code.svg"),
    ),
    (
        "icons/whole-word.svg",
        include_bytes!("../assets/icons/whole-word.svg"),
    ),
    (
        "icons/sparkles.svg",
        include_bytes!("../assets/icons/sparkles.svg"),
    ),
    (
        "icons/settings-2.svg",
        include_bytes!("../assets/icons/settings-2.svg"),
    ),
    (
        "icons/list-tree.svg",
        include_bytes!("../assets/icons/list-tree.svg"),
    ),
    (
        "icons/circle-help.svg",
        include_bytes!("../assets/icons/circle-help.svg"),
    ),
    ("icons/x.svg", include_bytes!("../assets/icons/x.svg")),
    (
        "icons/image.svg",
        include_bytes!("../assets/icons/image.svg"),
    ),
    (
        "icons/chevron-down.svg",
        include_bytes!("../assets/icons/chevron-down.svg"),
    ),
    (
        "icons/chevron-right.svg",
        include_bytes!("../assets/icons/chevron-right.svg"),
    ),
    (
        "icons/alert-triangle.svg",
        include_bytes!("../assets/icons/alert-triangle.svg"),
    ),
    // Per-adapter agent tab glyphs. Filenames are the agent registry slugs
    // (not vendor names); monochrome `currentColor` so they tint with the
    // tab's active/inactive icon color.
    (
        "icons/claude-code.svg",
        include_bytes!("../assets/icons/claude-code.svg"),
    ),
    // Cursor + Amp marks for the onboarding agent picker (ACP presets have no
    // builtin adapter, so no icon shipped before this).
    ("icons/cursor.svg", include_bytes!("../assets/icons/cursor.svg")),
    ("icons/amp.svg", include_bytes!("../assets/icons/amp.svg")),
    ("icons/codex.svg", include_bytes!("../assets/icons/codex.svg")),
    ("icons/aider.svg", include_bytes!("../assets/icons/aider.svg")),
    // Import-provider glyphs for the history/resume picker (generic marks).
    (
        "icons/opencode.svg",
        include_bytes!("../assets/icons/opencode.svg"),
    ),
    (
        "icons/copilot.svg",
        include_bytes!("../assets/icons/copilot.svg"),
    ),
    ("icons/pi.svg", include_bytes!("../assets/icons/pi.svg")),
    (
        "icons/keyboard.svg",
        include_bytes!("../assets/icons/keyboard.svg"),
    ),
    // Browser tab glyphs: globe marks the tab kind; arrows drive back/forward.
    ("icons/globe.svg", include_bytes!("../assets/icons/globe.svg")),
    (
        "icons/arrow-left.svg",
        include_bytes!("../assets/icons/arrow-left.svg"),
    ),
    (
        "icons/arrow-right.svg",
        include_bytes!("../assets/icons/arrow-right.svg"),
    ),
    (
        "icons/arrow-up.svg",
        include_bytes!("../assets/icons/arrow-up.svg"),
    ),
    // Browser agent-context: camera captures a screenshot to the clipboard
    // (crosshair / file-code / list-tree, already registered above, drive the
    // element picker, DOM snapshot, and console copy).
    (
        "icons/camera.svg",
        include_bytes!("../assets/icons/camera.svg"),
    ),
    // Copy-confirmation check; devtools (wrench), page appearance (contrast),
    // and browser profile (user) toolbar controls.
    ("icons/check.svg", include_bytes!("../assets/icons/check.svg")),
    (
        "icons/wrench.svg",
        include_bytes!("../assets/icons/wrench.svg"),
    ),
    (
        "icons/contrast.svg",
        include_bytes!("../assets/icons/contrast.svg"),
    ),
    ("icons/user.svg", include_bytes!("../assets/icons/user.svg")),
    // Address-bar security indicator for https pages.
    ("icons/lock.svg", include_bytes!("../assets/icons/lock.svg")),
    // Pin marker for pinned workspace rows in the left rail.
    ("icons/pin.svg", include_bytes!("../assets/icons/pin.svg")),
    // Markdown editor view-mode toggle: `</>` (source) and a two-column glyph
    // (split). Preview reuses the bundled eye icon.
    ("icons/code.svg", include_bytes!("../assets/icons/code.svg")),
    (
        "icons/columns.svg",
        include_bytes!("../assets/icons/columns.svg"),
    ),
    // Voice-dictation mic button in the Agent Chat composer.
    ("icons/mic.svg", include_bytes!("../assets/icons/mic.svg")),
    // Voice-dictation history row: copy the transcript; trash removes a
    // downloaded speech model from the Speech-model dropdown.
    ("icons/copy.svg", include_bytes!("../assets/icons/copy.svg")),
    (
        "icons/trash.svg",
        include_bytes!("../assets/icons/trash.svg"),
    ),
];

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        for (asset_path, bytes) in APP_ICONS {
            if *asset_path == path {
                return Ok(Some(Cow::Borrowed(*bytes)));
            }
        }
        Ok(None)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(APP_ICONS
            .iter()
            .filter(|(asset_path, _)| asset_path.starts_with(path))
            .map(|(asset_path, _)| SharedString::from(*asset_path))
            .collect())
    }
}

/// Composite source: tries local `AppAssets` first; falls back to
/// `gpui_component_assets::Assets` for anything not shipped locally.
pub struct CompositeAssets;

impl AssetSource for CompositeAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match AppAssets.load(path)? {
            Some(bytes) => Ok(Some(bytes)),
            None => gpui_component_assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut out = AppAssets.list(path)?;
        out.extend(gpui_component_assets::Assets.list(path)?);
        Ok(out)
    }
}
