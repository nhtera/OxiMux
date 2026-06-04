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
/// file in `crates/app/assets/icons/` to register a new asset.
struct AppAssets;

/// SVGs shipped locally. Each entry is `(asset path, embedded bytes)`. To add
/// a new icon, drop the file under `crates/app/assets/icons/` and append a
/// tuple here — no other wiring needed.
const APP_ICONS: &[(&str, &[u8])] = &[
    (
        "icons/git-branch.svg",
        include_bytes!("../assets/icons/git-branch.svg"),
    ),
    (
        "icons/list-collapse.svg",
        include_bytes!("../assets/icons/list-collapse.svg"),
    ),
    (
        "icons/refresh-cw.svg",
        include_bytes!("../assets/icons/refresh-cw.svg"),
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
    ("icons/codex.svg", include_bytes!("../assets/icons/codex.svg")),
    ("icons/aider.svg", include_bytes!("../assets/icons/aider.svg")),
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
