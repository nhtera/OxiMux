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
