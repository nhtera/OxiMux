use super::*;

fn args(words: &str) -> Vec<String> {
    words.split_whitespace().map(str::to_owned).collect()
}

fn bad(tool: Tool, words: &str, worktree: &Path) -> bool {
    matches!(check(tool, &args(words), worktree), Err(SimErrorWire::BadInput(_)))
}

/// A worktree with a file and a folder in it, and a file beside it.
fn worktree() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("w");
    std::fs::create_dir_all(tree.join("build")).unwrap();
    std::fs::write(tree.join("build/app.apk"), b"apk").unwrap();
    std::fs::write(dir.path().join("secret.txt"), b"x").unwrap();
    (dir, tree)
}

#[test]
fn simctl_runs_only_the_allowlist() {
    let (_dir, w) = worktree();
    for ok in ["ui appearance dark", "listapps", "appinfo com.a.b", "terminate com.a.b", "openurl https://example.com/x", "status_bar override --time 9:41", "privacy grant photos com.a.b", "location set 10.7,106.7"] {
        assert_eq!(check(Tool::Simctl, &args(ok), &w).unwrap(), args(ok), "{ok}");
    }
    for removed in ["spawn sh", "io screenshot x.png", "addmedia a.png", "push com.a.b p.json", "keychain add-root-cert c.pem", "get_app_container com.a.b", "delete", "erase", "create x y", "clone x", "shutdown", "boot", "getenv HOME", "install App.app"] {
        assert!(bad(Tool::Simctl, removed, &w), "{removed}");
    }
    assert!(bad(Tool::Simctl, "", &w), "no command");
}

#[test]
fn simctl_reaches_the_attached_device_only() {
    let (_dir, w) = worktree();
    for other in ["erase all", "ui booted appearance dark", "ui all appearance dark", "terminate BOOTED com.a.b", "ui 81CE1BE8-E38A-4BA8-8AAB-5DACA07576B3 appearance dark", "listapps e0e0a1dd-2dac-448e-86d3-07b7f809bcd2"] {
        assert!(bad(Tool::Simctl, other, &w), "{other}");
    }
    for host in ["openurl file:///etc/passwd", "openurl FILE:///x", "privacy grant photos /tmp/x", "location set ~/x", "ui ../x", "ui ./x"] {
        assert!(bad(Tool::Simctl, host, &w), "{host}");
    }
}

#[test]
fn adb_refuses_its_global_options_and_device_wide_commands_anywhere() {
    let (_dir, w) = worktree();
    for ok in ["shell input keyevent 3", "uninstall com.a.b", "logcat -b crash", "emu geo fix 106.7 10.7", "shell am start -a android.intent.action.VIEW"] {
        assert_eq!(check(Tool::Adb, &args(ok), &w).unwrap(), args(ok), "{ok}");
    }
    for global in ["-s", "-t", "-d", "-e", "-H", "-P", "-L"] {
        assert!(bad(Tool::Adb, &format!("{global} X shell ls"), &w), "{global} first");
        assert!(bad(Tool::Adb, &format!("shell ls {global} X"), &w), "{global} later");
    }
    for glued in ["-sR58M123 shell", "shell -sR58", "-t2 shell", "-Hhost shell", "-P5037 shell", "-Ltcp:1 shell", "--one-device R58 shell"] {
        assert!(bad(Tool::Adb, glued, &w), "{glued}");
    }
    for refused in ["connect 10.0.0.2", "disconnect", "kill-server", "reboot", "root", "remount", "shell reboot", "shell su root", "devices", "forward tcp:1 tcp:2", "sync", "tcpip 5555", "start-server", "install-multiple a.apk"] {
        assert!(bad(Tool::Adb, refused, &w), "{refused}");
    }
}

#[test]
fn adb_host_paths_stay_inside_the_worktree() {
    let (dir, w) = worktree();
    let apk = std::fs::canonicalize(w.join("build/app.apk")).unwrap();
    let apk = apk.to_string_lossy().into_owned();
    // Relative to the worktree, made absolute.
    assert_eq!(check(Tool::Adb, &args("install -r build/app.apk"), &w).unwrap(), vec!["install".to_owned(), "-r".into(), apk.clone()]);
    assert_eq!(check(Tool::Adb, &args("push build/app.apk /sdcard/"), &w).unwrap()[1], apk);
    let pulled = check(Tool::Adb, &args("pull /sdcard/log.txt build/log.txt"), &w).unwrap();
    assert!(pulled[2].ends_with("build/log.txt") && Path::new(&pulled[2]).is_absolute(), "{pulled:?}");
    assert_eq!(pulled[1], "/sdcard/log.txt", "the device side is the device's");
    let into_root = check(Tool::Adb, &args("pull /sdcard/x build/.."), &w).unwrap();
    assert_eq!(Path::new(&into_root[2]), std::fs::canonicalize(&w).unwrap(), "into the worktree's own folder");
    // Out of it, by `..`, an absolute path, a home path, or a symlink.
    let secret = dir.path().join("secret.txt");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&secret, w.join("link.txt")).unwrap();
    for escape in [
        "push ../../etc/x /sdcard/".to_owned(),
        "push ../secret.txt /sdcard/".to_owned(),
        format!("push {} /sdcard/", secret.display()),
        "install ~/app.apk".to_owned(),
        "pull /sdcard/x ../out.txt".to_owned(),
        "pull /sdcard/x /tmp/out.txt".to_owned(),
        "pull /sdcard/x build/../../out.txt".to_owned(),
        "install missing.apk".to_owned(),
    ] {
        assert!(bad(Tool::Adb, &escape, &w), "{escape}");
    }
    #[cfg(unix)]
    assert!(bad(Tool::Adb, "push link.txt /sdcard/", &w), "a symlink out");
    // Without the other side, adb would read or write where the app runs.
    assert!(bad(Tool::Adb, "push build/app.apk", &w));
    assert!(bad(Tool::Adb, "pull /sdcard/x", &w));
}

#[test]
fn a_real_iphone_has_no_exec() {
    assert!(matches!(Tool::of(&DeviceId("iosdev:00008130-0001".into())), Err(SimErrorWire::Refused(_))));
    assert_eq!(Tool::of(&DeviceId("81CE1BE8-E38A-4BA8-8AAB-5DACA07576B3".into())).unwrap(), Tool::Simctl);
    assert_eq!(Tool::of(&DeviceId("avd:Pixel".into())).unwrap(), Tool::Adb);
    assert_eq!(Tool::of(&DeviceId("adb:R58M123".into())).unwrap(), Tool::Adb);
}

#[test]
fn the_question_shows_every_word_and_nothing_too_long() {
    let line = shown("adb", &["-s".into(), "emulator-5554".into(), "shell".into(), "echo hi; rm x".into(), "a\nb".into()]).unwrap();
    assert_eq!(line, r#"adb -s emulator-5554 shell "echo hi; rm x" "a\nb""#);
    assert!(matches!(shown("adb", &["x".repeat(SHOWN)]), Err(SimErrorWire::BadInput(_))));
}

#[test]
fn a_stopped_command_says_why_and_its_output_is_scrubbed() {
    let timed = output(runner::Bounded { status: None, stdout: b"partial".to_vec(), stderr: b"warn".to_vec(), truncated: false, timed_out: true });
    assert_eq!((timed.status, timed.stdout.as_str()), (None, "partial"));
    assert_eq!(timed.stderr, "warn\n[oximux: stopped after 30 s]");
    let cut = output(runner::Bounded { status: None, truncated: true, ..Default::default() });
    assert!(cut.truncated && cut.stderr.contains("cut at 64 KiB"));
    let key = "sk-ant-abc123XYZ";
    let scrubbed = output(runner::Bounded { status: Some(0), stdout: format!("token {key}").into_bytes(), ..Default::default() });
    assert!(!scrubbed.stdout.contains(key), "a secret-shaped value is scrubbed");
}
