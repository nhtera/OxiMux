//! The release feed: GitHub's `releases/latest` for this repo.
//!
//! `latest` (not the release list) is deliberate: the endpoint excludes
//! drafts and prereleases server-side, which matches the release workflow —
//! CI publishes a *draft* with the DMG attached so notes can be curated, and
//! nothing here can see it until it is published.

use serde::Deserialize;

use crate::version::Version;
use crate::UpdateError;

/// One asset naming scheme, enforced by CI (the release workflow refuses a
/// tag that disagrees with the workspace version, and uploads exactly this
/// filename). Anything else in the release is ignored.
fn expected_asset_name(version: Version) -> String {
    format!("OxiMux-{version}-macos-arm64.dmg")
}

/// A release worth acting on: the parsed tag and its one pinned asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub notes: String,
    pub asset_url: String,
    pub asset_size: u64,
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

/// Parse a `releases/latest` response body into the one release shape the
/// updater accepts.
pub fn parse_latest(body: &str) -> Result<Release, UpdateError> {
    let release: ApiRelease =
        serde_json::from_str(body).map_err(|err| UpdateError::Feed {
            detail: format!("unparseable release feed: {err}"),
        })?;

    let version = Version::parse(&release.tag_name).ok_or_else(|| UpdateError::Feed {
        detail: format!("release tag `{}` is not a version", release.tag_name),
    })?;

    // Exact-name pinning: a compromised feed can add assets, but only the one
    // whose name encodes this exact version is ever downloaded.
    let wanted = expected_asset_name(version);
    let asset = release
        .assets
        .into_iter()
        .find(|asset| asset.name == wanted)
        .ok_or_else(|| UpdateError::MissingAsset {
            name: wanted,
            tag: release.tag_name,
        })?;

    Ok(Release {
        version,
        notes: release.body.unwrap_or_default(),
        asset_url: asset.browser_download_url,
        asset_size: asset.size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(tag: &str, asset_name: &str) -> String {
        format!(
            r###"{{
              "tag_name": "{tag}",
              "body": "## What's new\n- things",
              "assets": [
                {{"name": "checksums.txt", "browser_download_url": "https://example.com/c", "size": 100}},
                {{"name": "{asset_name}", "browser_download_url": "https://github.com/nhtera/OxiMux/releases/download/{tag}/{asset_name}", "size": 52428800}}
              ]
            }}"###
        )
    }

    #[test]
    fn picks_the_exactly_named_dmg_for_the_tag() {
        let release =
            parse_latest(&feed("v0.2.0", "OxiMux-0.2.0-macos-arm64.dmg")).expect("parses");
        assert_eq!(release.version, Version::new(0, 2, 0));
        assert_eq!(release.asset_size, 52_428_800);
        assert!(release.notes.contains("What's new"));
        assert!(release.asset_url.ends_with("OxiMux-0.2.0-macos-arm64.dmg"));
    }

    #[test]
    fn a_tag_asset_name_mismatch_is_rejected() {
        // The rollback defense: a fabricated high tag pointing at an old
        // build's asset fails the exact-name pin before any download.
        let err = parse_latest(&feed("v9.9.9", "OxiMux-0.1.0-macos-arm64.dmg"))
            .expect_err("must reject");
        assert!(matches!(err, UpdateError::MissingAsset { .. }), "got {err:?}");
    }

    #[test]
    fn a_non_version_tag_is_rejected() {
        let err = parse_latest(&feed("nightly", "OxiMux-nightly-macos-arm64.dmg"))
            .expect_err("must reject");
        assert!(matches!(err, UpdateError::Feed { .. }), "got {err:?}");
    }

    #[test]
    fn missing_notes_default_to_empty() {
        let body = r#"{"tag_name": "v0.2.0", "assets": [
            {"name": "OxiMux-0.2.0-macos-arm64.dmg", "browser_download_url": "u", "size": 1}
        ]}"#;
        assert_eq!(parse_latest(body).expect("parses").notes, "");
    }
}
