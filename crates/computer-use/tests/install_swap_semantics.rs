//! Empirical proof for the installer's core assumption: replacing an app
//! bundle with atomic renames while a process from it is running does not
//! disturb that process — its open inode outlives the rename and the delete.
//! This is the reason the installer never needs to (and must not) stop the
//! shared driver daemon.
//!
//! The fixture is a `#!/bin/sh` script rather than a copied Mach-O: exec'ing a
//! freshly-written binary triggers macOS's first-run assessment (slow, and
//! observed hanging outright), while the kernel property under test — an open
//! vnode surviving rename/unlink of its directory — is identical for any open
//! file.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn renaming_a_bundle_out_from_under_a_running_process_leaves_it_running() {
    let root = tempfile::tempdir().expect("tempdir");
    let app = root.path().join("Fake.app");
    let bin_dir = app.join("Contents/MacOS");
    fs::create_dir_all(&bin_dir).expect("mkdir");
    let script = bin_dir.join("sleeper");
    fs::write(&script, "#!/bin/sh\nsleep 30\n").expect("write script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");

    let mut child = Command::new(&script)
        .spawn()
        .expect("spawn from the old bundle");
    // Let the shell reach its sleep so an exec-time failure can't masquerade
    // as surviving the swap.
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "fixture process must be running before the swap"
    );

    // The installer's swap: old aside, new in place — both atomic renames.
    let bak = root.path().join("Fake.app.bak");
    fs::rename(&app, &bak).expect("set old aside");
    let replacement = root.path().join("Fake.app.staging");
    fs::create_dir_all(replacement.join("Contents/MacOS")).expect("mkdir");
    fs::rename(&replacement, &app).expect("new into place");
    // And the commit: the old bundle — the running child's inode — is deleted.
    fs::remove_dir_all(&bak).expect("discard old");

    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "process must survive its bundle being swapped and deleted"
    );
    let _ = child.kill();
    let _ = child.wait();
}
