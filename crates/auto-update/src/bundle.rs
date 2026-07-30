//! Self-inspection: is this process an updatable install, and what identity
//! must an update prove?

use std::path::{Path, PathBuf};

use oximux_macos_trust::SignaturePolicy;

/// Why the updater is disabled for this process. Each variant surfaces in
/// settings, so they name the actual obstacle rather than a generic "no".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnsupportedReason {
    /// `cargo run` / bare binary — nothing to swap.
    NotABundle,
    /// Gatekeeper app translocation: the bundle path is a read-only mirror,
    /// not the real install.
    Translocated,
    /// The directory holding the bundle refuses writes (e.g. `/Applications`
    /// owned by another admin account).
    RootNotWritable,
    /// Unsigned or ad-hoc/Apple-signed (`TeamIdentifier=not set`): there is
    /// no publisher identity to pin updates to, and pinning to "not set"
    /// would accept anyone's bundle.
    NoPinnableSignature,
}

impl UnsupportedReason {
    pub fn describe(&self) -> &'static str {
        match self {
            Self::NotABundle => "updates are unavailable in development builds",
            Self::Translocated => {
                "move OxiMux to Applications to enable updates (macOS is running a translocated copy)"
            }
            Self::RootNotWritable => "can't write next to the installed app",
            Self::NoPinnableSignature => "this build isn't signed for updates",
        }
    }
}

/// An install the updater may act on: the bundle on disk and the signature
/// pin every downloaded update must match.
///
/// Capture this ONCE at boot and keep it for the process lifetime. The pin
/// must describe the *running* app; re-reading it later would read whatever
/// currently sits at the bundle path — after a staged update lands, that is
/// the new bundle, and a drifting anchor defeats the point of pinning.
#[derive(Debug, Clone)]
pub struct InstalledApp {
    pub bundle_root: PathBuf,
    pub pin: SignaturePolicy,
}

/// Classify `exe` (normally `std::env::current_exe()`) as updatable or not.
///
/// Runs one `codesign -dvv` (~sub-second) plus a write probe; call it off the
/// UI thread.
pub fn eligibility(exe: &Path) -> Result<InstalledApp, UnsupportedReason> {
    let bundle_root = bundle_root_of(exe).ok_or(UnsupportedReason::NotABundle)?;

    if bundle_root
        .components()
        .any(|c| c.as_os_str() == "AppTranslocation")
    {
        return Err(UnsupportedReason::Translocated);
    }

    let parent = bundle_root
        .parent()
        .ok_or(UnsupportedReason::RootNotWritable)?;
    if !dir_is_writable(parent) {
        return Err(UnsupportedReason::RootNotWritable);
    }

    let signature = oximux_macos_trust::read_signature(&bundle_root)
        .map_err(|_| UnsupportedReason::NoPinnableSignature)?;
    if !signature.pinnable() {
        return Err(UnsupportedReason::NoPinnableSignature);
    }

    Ok(InstalledApp {
        bundle_root,
        pin: SignaturePolicy {
            identifier: signature.identifier,
            team_id: signature.team_id,
        },
    })
}

/// The `.app` root above a `Contents/MacOS/<exe>` path, or `None` when the
/// executable does not live in a bundle (dev builds).
pub fn bundle_root_of(exe: &Path) -> Option<PathBuf> {
    let macos_dir = exe.parent()?;
    if macos_dir.file_name()? != "MacOS" {
        return None;
    }
    let contents = macos_dir.parent()?;
    if contents.file_name()? != "Contents" {
        return None;
    }
    let root = contents.parent()?;
    if root.extension()? != "app" {
        return None;
    }
    Some(root.to_path_buf())
}

/// Probed by creating a file — directory permission bits under-report on
/// macOS.
fn dir_is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".oximux-write-probe-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    if ok {
        let _ = std::fs::remove_file(&probe);
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bundle_exe_resolves_to_its_app_root() {
        let root = bundle_root_of(Path::new(
            "/Applications/OxiMux.app/Contents/MacOS/oximux",
        ));
        assert_eq!(root, Some(PathBuf::from("/Applications/OxiMux.app")));
    }

    #[test]
    fn a_dev_build_path_is_not_a_bundle() {
        for exe in [
            "/Users/dev/proj/target/debug/oximux",
            "/usr/local/bin/oximux",
            "/Applications/NotAnApp/Contents/MacOS/oximux",
        ] {
            assert_eq!(bundle_root_of(Path::new(exe)), None, "{exe}");
        }
    }

    #[test]
    fn a_translocated_bundle_is_refused() {
        // Translocated paths look like real bundles; only the marker
        // component gives them away. eligibility() must catch it before the
        // (nonexistent) path fails some later, vaguer check.
        let exe = Path::new(
            "/private/var/folders/ab/xyz/T/AppTranslocation/1234/d/OxiMux.app/Contents/MacOS/oximux",
        );
        assert_eq!(
            eligibility(exe).expect_err("must refuse"),
            UnsupportedReason::Translocated
        );
    }

    #[test]
    fn every_reason_has_user_facing_copy() {
        for reason in [
            UnsupportedReason::NotABundle,
            UnsupportedReason::Translocated,
            UnsupportedReason::RootNotWritable,
            UnsupportedReason::NoPinnableSignature,
        ] {
            assert!(!reason.describe().is_empty());
        }
    }
}
