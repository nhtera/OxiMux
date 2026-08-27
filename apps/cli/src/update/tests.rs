//! The pipeline end to end, against a fetcher that serves bytes from a map.
//!
//! What these assert is *ordering*, which is the part of a trust chain that
//! silently rots: a gate that runs after the thing it was supposed to guard is
//! still a gate, still passes its own unit test, and protects nothing. So each
//! refusal test checks not only that the update failed but that the step it
//! was meant to prevent never happened — no archive requested, no binary
//! touched.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use oximux_auto_update::release::testkit::MinisignKeypair;
use super::*;

const TARGET: &str = "x86_64-unknown-linux-gnu";
const RUNNING: &str = "0.1.6";

/// Serves canned bytes and records every URL asked for.
#[derive(Default)]
struct MapFetcher {
    files: HashMap<String, Vec<u8>>,
    asked: RefCell<Vec<String>>,
}

impl MapFetcher {
    fn asked_for_an_archive(&self) -> bool {
        self.asked.borrow().iter().any(|url| url.ends_with(".tar.gz"))
    }
}

impl Fetcher for MapFetcher {
    fn get(&self, url: &str, ceiling: u64) -> Result<Vec<u8>, UpdateError> {
        self.asked.borrow_mut().push(url.to_string());
        let body = self.files.get(url).cloned().ok_or(UpdateError::NoRelease)?;
        if body.len() as u64 > ceiling {
            return Err(UpdateError::Oversize { ceiling });
        }
        Ok(body)
    }
}

/// A signed release: manifest, signature, and the archive the manifest names.
struct Release {
    fetcher: MapFetcher,
    public_key: String,
}

impl Release {
    fn build(version: &str, sign_with: Option<&MinisignKeypair>, corrupt_archive: bool) -> Self {
        let keys = MinisignKeypair::generate();
        let archive = archive::tar_gz(&[
            (&cli_name(), format!("cli {version}").as_bytes()),
            (&relay_name(), format!("relay {version}").as_bytes()),
        ]);
        let honest_digest = verify::sha256_hex(&archive);
        let served = if corrupt_archive { b"tampered bytes".to_vec() } else { archive };

        let name = format!("oximux-{version}-{TARGET}.tar.gz");
        let raw = format!(
            r#"{{"schemaVersion":1,"version":"{version}","channel":"stable","targets":{{"{TARGET}":{{"archive":"{name}","size":{},"sha256":"{honest_digest}"}}}}}}"#,
            served.len().max(1)
        );
        let signature = sign_with.unwrap_or(&keys).sign(raw.as_bytes());

        let mut files = HashMap::new();
        files.insert(download::manifest_url(), raw.into_bytes());
        files.insert(download::signature_url(), signature.into_bytes());
        files.insert(download::asset_url(&format!("v{version}"), &name), served);
        Self { fetcher: MapFetcher { files, asked: RefCell::default() }, public_key: keys.public_key_base64() }
    }

    fn signed(version: &str) -> Self {
        Self::build(version, None, false)
    }
}

/// Two "installed" binaries in a temp dir.
fn installed(dir: &Path) -> Install {
    std::fs::write(dir.join(cli_name()), "cli 0.1.6").expect("write cli");
    std::fs::write(dir.join(relay_name()), "relay 0.1.6").expect("write relay");
    Install::at(&dir.join(cli_name())).expect("discoverable")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).expect("readable")
}

#[test]
fn a_signed_release_replaces_both_binaries() {
    let dir = tempfile::tempdir().expect("tempdir");
    let install = installed(dir.path());
    let release = Release::signed("0.2.0");

    let applied =
        apply(&release.fetcher, Some(&release.public_key), RUNNING, TARGET, &install)
            .expect("updates");

    assert_eq!(applied.from, RUNNING);
    assert_eq!(applied.to, "0.2.0");
    assert_eq!(read(&install.cli), "cli 0.2.0");
    assert_eq!(read(&install.relay), "relay 0.2.0");
}

/// The staging directory must not survive a successful run either — a leftover
/// verified binary beside the installed one is a swap waiting to happen.
#[test]
fn nothing_is_left_in_the_install_directory_afterwards() {
    let dir = tempfile::tempdir().expect("tempdir");
    let install = installed(dir.path());
    let release = Release::signed("0.2.0");
    apply(&release.fetcher, Some(&release.public_key), RUNNING, TARGET, &install)
        .expect("updates");

    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .expect("readdir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|name| name != &cli_name() && name != &relay_name())
        .collect();
    assert!(leftovers.is_empty(), "unexpected leftovers: {leftovers:?}");
}

/// The criterion checksums alone cannot meet. The manifest is internally
/// consistent — its sha256 really is the archive's — and it is still refused,
/// because it is not signed by the key this build trusts.
#[test]
fn a_manifest_signed_by_another_key_is_refused_before_anything_is_downloaded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let install = installed(dir.path());
    let attacker = MinisignKeypair::generate();
    let release = Release::build("9.9.9", Some(&attacker), false);

    let err = apply(&release.fetcher, Some(&release.public_key), RUNNING, TARGET, &install)
        .expect_err("must refuse");

    assert!(matches!(err, UpdateError::Verify(verify::VerifyError::BadSignature)), "{err}");
    assert!(!release.fetcher.asked_for_an_archive(), "the archive must never be requested");
    assert_eq!(read(&install.cli), "cli 0.1.6", "the installed binary is untouched");
}

/// Signature good, bytes swapped underneath it. The digest gate catches it,
/// and it must catch it before anything is extracted or swapped.
#[test]
fn an_archive_that_does_not_match_the_signed_digest_never_reaches_the_swap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let install = installed(dir.path());
    let release = Release::build("0.2.0", None, true);

    let err = apply(&release.fetcher, Some(&release.public_key), RUNNING, TARGET, &install)
        .expect_err("must refuse");

    assert!(matches!(err, UpdateError::Verify(verify::VerifyError::DigestMismatch { .. })), "{err}");
    assert_eq!(read(&install.cli), "cli 0.1.6");
    assert_eq!(read(&install.relay), "relay 0.1.6");
}

/// A correctly signed *old* release must not walk an installation backwards
/// onto a fixed bug. Signed replay is the attack; the version gate is the
/// answer.
#[test]
fn a_correctly_signed_older_release_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let install = installed(dir.path());
    let release = Release::signed("0.1.2");

    let err = apply(&release.fetcher, Some(&release.public_key), RUNNING, TARGET, &install)
        .expect_err("must refuse");

    assert!(
        matches!(err, UpdateError::Verify(verify::VerifyError::NotAnUpgrade { .. })),
        "{err}"
    );
    assert!(!release.fetcher.asked_for_an_archive(), "a downgrade must not even be downloaded");
    assert_eq!(read(&install.cli), "cli 0.1.6");
}

#[test]
fn the_same_version_is_not_an_update() {
    let dir = tempfile::tempdir().expect("tempdir");
    let install = installed(dir.path());
    let release = Release::signed(RUNNING);
    let err = apply(&release.fetcher, Some(&release.public_key), RUNNING, TARGET, &install)
        .expect_err("must refuse");
    assert!(matches!(err, UpdateError::Verify(verify::VerifyError::NotAnUpgrade { .. })), "{err}");
}

/// Without a trust root there is nothing to verify against, so the correct
/// behaviour is to make no request at all rather than fetch bytes it could
/// only either trust blindly or throw away.
#[test]
fn a_build_with_no_release_key_never_reaches_the_network() {
    let dir = tempfile::tempdir().expect("tempdir");
    let install = installed(dir.path());
    let release = Release::signed("0.2.0");

    let err = apply(&release.fetcher, None, RUNNING, TARGET, &install).expect_err("must refuse");

    assert!(matches!(err, UpdateError::Verify(verify::VerifyError::NoTrustRoot)), "{err}");
    assert!(release.fetcher.asked.borrow().is_empty(), "no request may be made");
    assert!(into_failure(err).next_steps.iter().any(|s| s.contains("install-cli.sh")));
}

/// A platform the release skipped must not download anything, and must say
/// what the release does carry.
#[test]
fn a_platform_the_release_skipped_is_named_not_guessed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let install = installed(dir.path());
    let release = Release::signed("0.2.0");

    let err = apply(
        &release.fetcher,
        Some(&release.public_key),
        RUNNING,
        "powerpc64-unknown-linux-gnu",
        &install,
    )
    .expect_err("must refuse");

    assert!(err.to_string().contains(TARGET), "{err}");
    assert!(!release.fetcher.asked_for_an_archive());
}

/// Fighting a package manager produces an installation neither side can
/// reason about. The answer is its command, not ours.
#[test]
fn a_homebrew_install_is_told_to_use_brew() {
    let err = Install::at(Path::new("/opt/homebrew/Cellar/oximux/0.1.6/bin/oximux"))
        .expect_err("must refuse");
    assert!(matches!(err, UpdateError::ManagedInstall { manager: "Homebrew", .. }), "{err}");
    assert!(into_failure(err).next_steps.iter().any(|s| s.contains("brew upgrade")));
}

#[test]
fn an_ordinary_install_is_not_mistaken_for_a_managed_one() {
    let install = Install::at(Path::new("/home/dev/.local/bin/oximux")).expect("plain install");
    assert_eq!(install.dir, Path::new("/home/dev/.local/bin"));
    assert_eq!(install.relay, Path::new("/home/dev/.local/bin").join(relay_name()));
}

/// A verification failure must read as "something is wrong with the release",
/// not as a transient network blip a script would retry through.
#[test]
fn a_verification_failure_is_not_classified_as_unreachable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let install = installed(dir.path());
    let attacker = MinisignKeypair::generate();
    let release = Release::build("9.9.9", Some(&attacker), false);
    let failure = into_failure(
        apply(&release.fetcher, Some(&release.public_key), RUNNING, TARGET, &install)
            .expect_err("must refuse"),
    );

    assert_eq!(failure.code, "untrusted");
    assert_eq!(failure.exit, exit::ERROR, "not exit 3 — retrying will not help");
    assert!(failure.next_steps.iter().any(|s| s.contains("Do not retry blindly")));
}

/// `--check` must read the release without touching the installation.
#[test]
fn checking_reads_the_manifest_and_changes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let install = installed(dir.path());
    let release = Release::signed("0.3.1");

    let manifest = fetch_verified_manifest(&release.fetcher, Some(&release.public_key))
        .expect("verified");

    assert_eq!(manifest.version, "0.3.1");
    assert_eq!(manifest.tag(), "v0.3.1");
    assert!(!release.fetcher.asked_for_an_archive());
    assert_eq!(read(&install.cli), "cli 0.1.6");
}
