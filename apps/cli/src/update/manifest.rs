//! The release manifest: what versions exist and what their artifacts hash to.
//!
//! The manifest is the *only* thing signed. Everything downstream — which
//! archive to fetch, what it must hash to, how big it may be — is read out of
//! it, which is why [`super::verify`] checks the signature over the raw bytes
//! before this module is allowed to parse them.
//!
//! Asset URLs are deliberately **not** in the manifest. They are derived from
//! a fixed base, the signed version, and the signed file name, so a manifest
//! cannot point the downloader at a host of its choosing even if the signing
//! key is one day misused. The host allow-list downstream stays meaningful.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The schema this build understands. A manifest declaring anything else is
/// refused rather than best-effort parsed: a future field this build silently
/// drops could be the one carrying a new verification requirement.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    /// `0.1.7`, no `v`. The tag is `v` + this.
    pub version: String,
    pub channel: String,
    /// Target triple → the one archive built for it.
    pub targets: BTreeMap<String, Asset>,
}

/// One release archive. `sha256` is lowercase hex; `size` bounds the download
/// so a tarpit cannot stream forever before the hash is ever computed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub archive: String,
    pub size: u64,
    pub sha256: String,
}

/// Why a manifest was refused. Every variant is a refusal — there is no
/// "parsed with warnings" state, because a manifest this build only partly
/// understands is exactly the case where proceeding is unsafe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    Malformed(String),
    UnknownSchema { found: u32 },
    NoAssetForTarget { target: String, available: Vec<String> },
    BadDigest { found: String },
    /// The name would escape the directory it is extracted into, or is not a
    /// plain file name at all.
    UnsafeAssetName { name: String },
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(detail) => write!(f, "the release manifest could not be read: {detail}"),
            Self::UnknownSchema { found } => write!(
                f,
                "the release manifest declares schema {found}, and this build only understands \
                 {SCHEMA_VERSION} — install a newer oximux to update further"
            ),
            Self::NoAssetForTarget { target, available } => write!(
                f,
                "the release has no build for {target} (it has: {})",
                if available.is_empty() { "nothing".to_string() } else { available.join(", ") }
            ),
            Self::BadDigest { found } => {
                write!(f, "the manifest carries a malformed sha256: {found:?}")
            }
            Self::UnsafeAssetName { name } => {
                write!(f, "the manifest names an unsafe archive path: {name:?}")
            }
        }
    }
}

impl Manifest {
    /// Parse **verified** bytes. Callers must have checked the signature
    /// first; this function has no way to tell whether they did, which is why
    /// the only caller is [`super::fetch_verified_manifest`].
    pub fn parse(raw: &[u8]) -> Result<Self, ManifestError> {
        let manifest: Self = serde_json::from_slice(raw)
            .map_err(|err| ManifestError::Malformed(err.to_string()))?;
        if manifest.schema_version != SCHEMA_VERSION {
            return Err(ManifestError::UnknownSchema { found: manifest.schema_version });
        }
        if manifest.version.trim().is_empty() {
            return Err(ManifestError::Malformed("no version".into()));
        }
        for asset in manifest.targets.values() {
            asset.validate()?;
        }
        Ok(manifest)
    }

    /// The asset built for `target`, or a refusal naming what the release
    /// does have — "no build for aarch64-unknown-linux-musl" with the list
    /// beside it is actionable; "not found" is not.
    pub fn asset_for(&self, target: &str) -> Result<&Asset, ManifestError> {
        self.targets.get(target).ok_or_else(|| ManifestError::NoAssetForTarget {
            target: target.to_string(),
            available: self.targets.keys().cloned().collect(),
        })
    }

    pub fn tag(&self) -> String {
        format!("v{}", self.version)
    }
}

impl Asset {
    fn validate(&self) -> Result<(), ManifestError> {
        let digest = self.sha256.trim();
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ManifestError::BadDigest { found: self.sha256.clone() });
        }
        // The name becomes a path component in the download directory and a
        // URL segment. Anything with a separator, a parent hop, or a leading
        // dash (which an extractor could read as a flag) is refused.
        let name = &self.archive;
        let unsafe_name = name.is_empty()
            || name.starts_with('-')
            || name.contains('/')
            || name.contains('\\')
            || name.contains("..")
            || std::path::Path::new(name).file_name().map(|f| f != name.as_str()) != Some(false);
        if unsafe_name {
            return Err(ManifestError::UnsafeAssetName { name: name.clone() });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_json(version: &str, target: &str, sha: &str) -> String {
        format!(
            r#"{{"schemaVersion":1,"version":"{version}","channel":"stable","targets":{{
                "{target}":{{"archive":"oximux-{version}-{target}.tar.gz","size":100,"sha256":"{sha}"}}
            }}}}"#
        )
    }

    const SHA: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    #[test]
    fn parses_a_well_formed_manifest_and_finds_this_targets_asset() {
        let raw = manifest_json("0.2.0", "aarch64-apple-darwin", SHA);
        let manifest = Manifest::parse(raw.as_bytes()).expect("parses");
        assert_eq!(manifest.version, "0.2.0");
        assert_eq!(manifest.tag(), "v0.2.0");
        let asset = manifest.asset_for("aarch64-apple-darwin").expect("has the asset");
        assert_eq!(asset.archive, "oximux-0.2.0-aarch64-apple-darwin.tar.gz");
    }

    /// The failure a user actually hits — a platform the release skipped —
    /// must say what the release *does* have, not just "missing".
    #[test]
    fn a_missing_target_names_what_the_release_does_carry() {
        let raw = manifest_json("0.2.0", "aarch64-apple-darwin", SHA);
        let manifest = Manifest::parse(raw.as_bytes()).expect("parses");
        let err = manifest.asset_for("powerpc-unknown-linux-gnu").expect_err("no such target");
        let rendered = err.to_string();
        assert!(rendered.contains("powerpc-unknown-linux-gnu"), "{rendered}");
        assert!(rendered.contains("aarch64-apple-darwin"), "{rendered}");
    }

    /// The exact bytes `.github/workflows/release.yml` emits, pinned here.
    ///
    /// The generator is `jq` in a shell script and the parser is serde: nothing
    /// but this test connects them. `schemaVersion` in particular is camelCase
    /// on the wire and snake_case in Rust, which is the kind of detail a
    /// well-meaning edit to either side breaks without any other test noticing
    /// — and the symptom would be every `oximux update` in the world reporting
    /// a malformed manifest, after the release is already published.
    #[test]
    fn the_manifest_the_release_workflow_generates_is_the_one_this_parses() {
        let from_ci = r#"{"schemaVersion":1,"version":"0.1.6","channel":"stable","targets":{"aarch64-apple-darwin":{"archive":"oximux-0.1.6-aarch64-apple-darwin.tar.gz","size":37,"sha256":"5502bb9914bd6697b8c58d60baa7b4b7ecb61c01939c161fbd17b3fec14bd2cb"},"x86_64-pc-windows-msvc":{"archive":"oximux-0.1.6-x86_64-pc-windows-msvc.tar.gz","size":39,"sha256":"e8116cbfe68d3a7082a3c1b1cf8801c750596820ded6ab94ab45b160327c300d"}}}"#;

        let manifest = Manifest::parse(from_ci.as_bytes()).expect("the workflow's own output parses");
        assert_eq!(manifest.version, "0.1.6");
        assert_eq!(manifest.channel, "stable");
        assert_eq!(manifest.tag(), "v0.1.6");
        let asset = manifest.asset_for("aarch64-apple-darwin").expect("its target");
        assert_eq!(asset.archive, "oximux-0.1.6-aarch64-apple-darwin.tar.gz");
        assert_eq!(asset.size, 37);
    }

    /// A newer manifest must stop an older binary rather than be parsed
    /// leniently: the field this build would drop could be the one carrying a
    /// new verification requirement.
    #[test]
    fn an_unknown_schema_is_refused_not_best_effort_parsed() {
        let raw = r#"{"schemaVersion":2,"version":"9.9.9","channel":"stable","targets":{}}"#;
        assert_eq!(
            Manifest::parse(raw.as_bytes()),
            Err(ManifestError::UnknownSchema { found: 2 })
        );
    }

    #[test]
    fn a_malformed_digest_is_refused_at_parse_time() {
        for bad in ["", "abc", &"z".repeat(64), &"a".repeat(63)] {
            let raw = manifest_json("0.2.0", "x86_64-unknown-linux-gnu", bad);
            let err = Manifest::parse(raw.as_bytes()).expect_err("must refuse {bad}");
            assert!(matches!(err, ManifestError::BadDigest { .. }), "{bad:?} -> {err:?}");
        }
    }

    /// The archive name lands on disk and in a URL. A traversal or a
    /// leading-dash name must die in the parser, before either happens.
    ///
    /// The fixture is *built* rather than written out, so that a name
    /// containing a backslash produces the JSON escape a real hostile manifest
    /// would carry. Interpolating one into hand-written JSON yields an invalid
    /// escape instead, and the parser then rejects it as malformed — passing
    /// the assertion for the wrong reason and never reaching the name check.
    #[test]
    fn an_archive_name_can_never_escape_the_download_directory() {
        for bad in ["../evil.tar.gz", "a/b.tar.gz", "..", "-rf.tar.gz", "", "sub\\evil.tar.gz"] {
            let raw = serde_json::json!({
                "schemaVersion": 1,
                "version": "0.2.0",
                "channel": "stable",
                "targets": { "t": { "archive": bad, "size": 1, "sha256": SHA } },
            })
            .to_string();
            let err = Manifest::parse(raw.as_bytes()).expect_err("must refuse");
            assert!(matches!(err, ManifestError::UnsafeAssetName { .. }), "{bad:?} -> {err:?}");
        }
    }
}
