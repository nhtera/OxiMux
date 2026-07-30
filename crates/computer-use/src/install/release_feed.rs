//! Picking the newest driver release out of trycua's mixed release feed.
//!
//! The upstream repo publishes several components (driver, lume, fleet) as
//! releases of one repository, so GitHub's "latest release" endpoint answers
//! for whichever component shipped last. The only reliable selector is the
//! tag prefix; everything here is pure parsing so it stays testable offline.

use serde::Deserialize;

use super::InstallError;
use crate::version::Version;

/// Listing endpoint. 30 releases is weeks of upstream history — enough that a
/// driver release is always in the window even with other components shipping.
pub const RELEASES_URL: &str = "https://api.github.com/repos/trycua/cua/releases?per_page=30";

/// Driver releases are tagged `cua-driver-rs-vX.Y.Z`.
const TAG_PREFIX: &str = "cua-driver-rs-v";

/// Transport-integrity manifest shipped in every driver release.
pub const CHECKSUMS_ASSET: &str = "checksums.txt";

#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    /// Redirects to GitHub's asset CDN when fetched; only ever taken from the
    /// typed API response, never from page content.
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    // No `prerelease` field on purpose: upstream sets it on every driver
    // release, so reading it would only invite filtering on it again.
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

/// A driver release with the two assets the installer needs.
#[derive(Debug, Clone)]
pub struct DriverRelease {
    pub version: Version,
    pub tarball: ReleaseAsset,
    pub checksums: ReleaseAsset,
}

/// The macOS asset carrying `CuaDriver.app` (alongside the bare binary).
pub fn tarball_name(version: &Version) -> String {
    format!("cua-driver-rs-{version}-darwin-universal.tar.gz")
}

/// Newest published driver release in a raw `/releases` response.
///
/// The feed is ordered by publish date, not by version — the downgrade guard
/// against an *installed* driver lives in the pipeline, where the installed
/// version is known.
///
/// Drafts are skipped; the `prerelease` flag is deliberately ignored —
/// upstream marks **every** driver release prerelease (verified against the
/// live feed, 2026-07-30), so filtering on it selects nothing. The version
/// floor and signature gates carry the actual quality bar.
pub fn parse_latest(json: &str) -> Result<DriverRelease, InstallError> {
    let releases: Vec<Release> =
        serde_json::from_str(json).map_err(|err| InstallError::Feed {
            detail: format!("release feed did not parse: {err}"),
        })?;

    let release = releases
        .iter()
        .find(|release| !release.draft && release.tag_name.starts_with(TAG_PREFIX))
        .ok_or(InstallError::NoDriverRelease)?;

    let version = Version::parse(&release.tag_name[TAG_PREFIX.len()..]).ok_or_else(|| {
        InstallError::Feed {
            detail: format!("unparseable driver tag `{}`", release.tag_name),
        }
    })?;

    let find = |name: &str| {
        release
            .assets
            .iter()
            .find(|asset| asset.name == name)
            .cloned()
            .ok_or_else(|| InstallError::MissingAsset {
                name: name.to_string(),
                tag: release.tag_name.clone(),
            })
    };

    Ok(DriverRelease {
        tarball: find(&tarball_name(&version))?,
        checksums: find(CHECKSUMS_ASSET)?,
        version,
    })
}

/// Expected hash for `asset_name` in a `checksums.txt` body
/// (`<hex>  <filename>` lines; a leading `*` marks binary mode).
pub fn expected_sha256(checksums: &str, asset_name: &str) -> Option<String> {
    checksums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?;
        (name.trim_start_matches('*') == asset_name).then(|| hash.to_ascii_lowercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> String {
        format!(
            r#"{{"name":"{name}","browser_download_url":"https://github.com/trycua/cua/releases/download/x/{name}","size":100}}"#
        )
    }

    fn release(tag: &str, flags: &str, assets: &[&str]) -> String {
        let assets: Vec<String> = assets.iter().map(|a| asset(a)).collect();
        format!(
            r#"{{"tag_name":"{tag}",{flags}"assets":[{}]}}"#,
            assets.join(",")
        )
    }

    #[test]
    fn skips_other_components_and_drafts_but_accepts_prereleases() {
        // Mirrors the live feed shape: lume/fleet tags interleave with driver
        // tags, unpublished drafts appear, and — verified live — every driver
        // release carries `prerelease: true`, so that flag must not filter.
        let feed = format!(
            "[{},{},{}]",
            release("lume-v0.5.0", "", &["lume.tar.gz"]),
            release("cua-driver-rs-v0.15.0", r#""draft":true,"#, &[]),
            release(
                "cua-driver-rs-v0.14.1",
                r#""prerelease":true,"#,
                &[
                    "cua-driver-rs-0.14.1-darwin-universal.tar.gz",
                    "checksums.txt"
                ]
            ),
        );
        let picked = parse_latest(&feed).expect("must find the published driver release");
        assert_eq!(picked.version, Version::new(0, 14, 1));
        assert!(picked.tarball.name.ends_with("darwin-universal.tar.gz"));
    }

    #[test]
    fn a_feed_with_no_driver_release_is_a_specific_error() {
        let feed = format!("[{}]", release("fleet-v0.0.5", "", &[]));
        assert!(matches!(
            parse_latest(&feed),
            Err(InstallError::NoDriverRelease)
        ));
    }

    #[test]
    fn a_driver_release_missing_its_tarball_names_the_asset() {
        let feed = format!(
            "[{}]",
            release("cua-driver-rs-v0.14.1", "", &["checksums.txt"])
        );
        match parse_latest(&feed) {
            Err(InstallError::MissingAsset { name, .. }) => {
                assert_eq!(name, "cua-driver-rs-0.14.1-darwin-universal.tar.gz");
            }
            other => panic!("expected MissingAsset, got {other:?}"),
        }
    }

    #[test]
    fn checksum_lookup_matches_exact_filename_and_binary_marker() {
        let body = "abc123  cua-driver-rs-0.14.1-darwin-universal.tar.gz\n\
                    DEF456 *other.zip\n";
        assert_eq!(
            expected_sha256(body, "cua-driver-rs-0.14.1-darwin-universal.tar.gz").as_deref(),
            Some("abc123")
        );
        assert_eq!(expected_sha256(body, "other.zip").as_deref(), Some("def456"));
        assert_eq!(expected_sha256(body, "missing.tar.gz"), None);
    }
}
