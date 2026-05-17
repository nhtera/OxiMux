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

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match path {
            "icons/git-branch.svg" => Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/icons/git-branch.svg"
            )))),
            _ => Ok(None),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        if "icons/git-branch.svg".starts_with(path) {
            Ok(vec!["icons/git-branch.svg".into()])
        } else {
            Ok(vec![])
        }
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
