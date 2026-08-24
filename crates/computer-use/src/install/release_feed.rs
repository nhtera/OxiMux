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
    /// The platform's archive: a `.tar.gz` carrying `CuaDriver.app` on macOS, a
    /// `.zip` of bare executables on Windows. Named for what it is to every
    /// platform rather than for the shape one of them happens to use.
    pub archive: ReleaseAsset,
    pub checksums: ReleaseAsset,
}

/// Newest published driver release in a raw `/releases` response.
///
/// The feed is ordered by publish date, not by version — the downgrade guard
/// against an *installed* driver lives in the pipeline, where the installed
/// version is known.
///
/// `asset_for` names the archive this platform installs from. It is a parameter
/// rather than a `cfg!` read inside here so that one fixture can be parsed for
/// *either* platform's asset on *any* host — a `cfg!` would make the macOS
/// asset name untestable on Windows and vice versa, which is precisely the
/// drift a cross-platform installer has to keep out.
///
/// Drafts are skipped; the `prerelease` flag is deliberately ignored —
/// upstream marks **every** driver release prerelease (verified against the
/// live feed, 2026-07-30), so filtering on it selects nothing. The version
/// floor and signature gates carry the actual quality bar.
pub fn parse_latest(
    json: &str,
    asset_for: fn(&Version) -> String,
) -> Result<DriverRelease, InstallError> {
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
        archive: find(&asset_for(&version))?,
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
    use crate::install::platform::{macos_asset_name, windows_asset_name};

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
        let picked =
            parse_latest(&feed, macos_asset_name).expect("must find the published driver release");
        assert_eq!(picked.version, Version::new(0, 14, 1));
        assert!(picked.archive.name.ends_with("darwin-universal.tar.gz"));
    }

    /// The cross-platform contract: one feed, either platform's archive, on
    /// whichever host happens to be running the test.
    #[test]
    fn the_same_release_resolves_each_platforms_own_archive() {
        let version = Version::new(0, 21, 0);
        let feed = format!(
            "[{}]",
            release(
                "cua-driver-rs-v0.21.0",
                r#""prerelease":true,"#,
                &[
                    &macos_asset_name(&version),
                    &windows_asset_name(&version),
                    "checksums.txt",
                ]
            )
        );

        let mac = parse_latest(&feed, macos_asset_name).expect("macOS archive");
        assert_eq!(mac.archive.name, macos_asset_name(&version));

        let win = parse_latest(&feed, windows_asset_name).expect("Windows archive");
        assert_eq!(win.archive.name, windows_asset_name(&version));
        assert!(win.archive.name.ends_with("-binary.zip"));
    }

    #[test]
    fn a_feed_with_no_driver_release_is_a_specific_error() {
        let feed = format!("[{}]", release("fleet-v0.0.5", "", &[]));
        assert!(matches!(
            parse_latest(&feed, macos_asset_name),
            Err(InstallError::NoDriverRelease)
        ));
    }

    #[test]
    fn a_driver_release_missing_its_archive_names_the_asset() {
        let feed = format!(
            "[{}]",
            release("cua-driver-rs-v0.14.1", "", &["checksums.txt"])
        );
        match parse_latest(&feed, macos_asset_name) {
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
