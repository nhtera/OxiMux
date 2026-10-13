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
        assert_eq!(check(Tool::Simctl, &args(ok), &w).unwrap().argv, args(ok), "{ok}");
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
    for ok in ["shell input keyevent 3", "uninstall com.a.b", "logcat -b crash", "emu geo fix 106.7 10.7", "emu sms send 123 hi", "emu network speed full", "shell am start -a android.intent.action.VIEW"] {
        assert_eq!(check(Tool::Adb, &args(ok), &w).unwrap().argv, args(ok), "{ok}");
    }
    for global in ["-s", "-t", "-d", "-e", "-H", "-P", "-L"] {
        assert!(bad(Tool::Adb, &format!("{global} X shell ls"), &w), "{global} first");
        assert!(bad(Tool::Adb, &format!("shell ls {global} X"), &w), "{global} later");
    }
    for glued in ["-sR58M123 shell", "shell -sR58", "-t2 shell", "-Hhost shell", "-P5037 shell", "-Ltcp:1 shell", "--one-device R58 shell"] {
        assert!(bad(Tool::Adb, glued, &w), "{glued}");
    }
    for refused in ["emu network capture start /tmp/x.pcap", "emu network", "shell -- ls", "push -- -x /sdcard/", "emu screenrecord start /tmp/x.webm", "emu avd snapshot save s", "emu kill", "emu", "connect 10.0.0.2", "disconnect", "kill-server", "reboot", "root", "remount", "shell reboot", "shell su root", "devices", "forward tcp:1 tcp:2", "sync", "tcpip 5555", "start-server", "install-multiple a.apk"] {
        assert!(bad(Tool::Adb, refused, &w), "{refused}");
    }
}

#[test]
fn adb_host_paths_stay_inside_the_worktree() {
    let (dir, w) = worktree();
    let apk = std::fs::canonicalize(w.join("build/app.apk")).unwrap();
    let apk = apk.to_string_lossy().into_owned();
    // Relative to the worktree, made absolute.
    let install = check(Tool::Adb, &args("install -r build/app.apk"), &w).unwrap();
    assert_eq!((install.argv, install.reads, install.writes), (vec!["install".to_owned(), "-r".into(), apk.clone()], vec![2], None));
    let push = check(Tool::Adb, &args("push build/app.apk /sdcard/"), &w).unwrap();
    assert_eq!((push.argv[1].as_str(), push.reads), (apk.as_str(), vec![1]));
    let pull = check(Tool::Adb, &args("pull /sdcard/a /sdcard/b build"), &w).unwrap();
    assert_eq!((pull.reads, pull.writes), (vec![], Some((3, 2))), "two device paths, written at word 3");
    let pulled = check(Tool::Adb, &args("pull /sdcard/log.txt build/log.txt"), &w).unwrap().argv;
    assert!(pulled[2].ends_with("build/log.txt") && Path::new(&pulled[2]).is_absolute(), "{pulled:?}");
    assert_eq!(pulled[1], "/sdcard/log.txt", "the device side is the device's");
    let into_root = check(Tool::Adb, &args("pull /sdcard/x build/.."), &w).unwrap().argv;
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
        // adb names what it writes after the device path's last word.
        "pull /sdcard/x/y/.. build".to_owned(),
        "pull /sdcard/./x build".to_owned(),
        // Never git's folder, in any case: a hook or config there runs code.
        "pull /sdcard/x .git/hooks/pre-commit".to_owned(),
        "pull /sdcard/x .GIT/config".to_owned(),
        "push .git/config /sdcard/".to_owned(),
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
    let timed = output((runner::Bounded { status: None, stdout: b"partial".to_vec(), stderr: b"warn".to_vec(), truncated: false, timed_out: true }, None));
    assert_eq!((timed.status, timed.stdout.as_str()), (None, "partial"));
    assert_eq!(timed.stderr, "warn\n[oximux: stopped after 30 s]");
    let cut = output((runner::Bounded { status: None, truncated: true, ..Default::default() }, None));
    assert!(cut.truncated && cut.stderr.contains("cut at 64 KiB"));
    let key = "sk-ant-abc123XYZ";
    let scrubbed = output((runner::Bounded { status: Some(0), stdout: format!("token {key}").into_bytes(), ..Default::default() }, None));
    assert!(!scrubbed.stdout.contains(key), "a secret-shaped value is scrubbed");
}

#[test]
fn not_there_and_not_inside_read_the_same() {
    let (dir, w) = worktree();
    let say = |words: &str| match check(Tool::Adb, &args(words), &w) {
        Err(SimErrorWire::BadInput(why)) => why.replace(words.split(' ').nth(1).unwrap(), "P"),
        other => panic!("{other:?}"),
    };
    let outside = format!("install {}", dir.path().join("secret.txt").display());
    assert_eq!(say(&outside), say("install /nonexistent/x.apk"), "nothing said about files elsewhere");
}

/// An approved transfer through a stand-in adb (a script): what it reads is
/// a private copy of the approved file, and a pull lands in the worktree only.
#[cfg(target_os = "macos")]
#[test]
fn a_transfer_goes_through_a_private_folder() {
    use std::os::unix::fs::PermissionsExt as _;
    let (dir, w) = worktree();
    let adb = dir.path().join("adb");
    // `-s S push|pull …`: push prints the file adb was given; pull writes a
    // folder with a file into the folder it was given.
    std::fs::write(&adb, "#!/bin/sh\ncase $3 in push) cat \"$4\"; echo \" $4\";; pull) mkdir \"$5/x\" && printf hi > \"$5/x/f.txt\" && chmod 755 \"$5/x/f.txt\";; esac\n").unwrap();
    std::fs::set_permissions(&adb, std::fs::Permissions::from_mode(0o755)).unwrap();
    let adb = adb.to_str().unwrap();
    let prefix = |c: &Checked| [vec!["-s".to_owned(), "S".into()], c.argv.clone()].concat();

    let push = check(Tool::Adb, &args("push build/app.apk /sdcard/"), &w).unwrap();
    let base = dir.path().join("private");
    let (done, placed) = transfer(adb, prefix(&push), 2, &push, &base).unwrap();
    let said = String::from_utf8_lossy(&done.stdout).into_owned();
    assert!(said.starts_with("apk ") && !said.contains("/w/build/"), "a copy, not the worktree file: {said}");
    assert_eq!(placed, None);

    std::fs::create_dir(w.join("out")).unwrap();
    let pull = check(Tool::Adb, &args("pull /sdcard/x out"), &w).unwrap();
    let (done, placed) = transfer(adb, prefix(&pull), 2, &pull, &base).unwrap();
    assert_eq!((done.status, placed), (Some(0), Some(1)));
    let file = w.join("out/x/f.txt");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "hi");
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o644, "never executable");
}

/// The worktree changed between the question and the answer: a symlink now
/// stands where the approved file or folder was. Nothing outside is read or
/// written.
#[cfg(target_os = "macos")]
#[test]
fn a_symlink_swapped_in_after_the_question_is_refused() {
    let (dir, w) = worktree();
    let root = std::fs::canonicalize(&w).unwrap();
    let outside = dir.path().join("secret.txt");
    // Read: the approved file is now a link out.
    let apk = root.join("build/app.apk");
    let into = tempfile::tempdir().unwrap();
    std::fs::remove_file(&apk).unwrap();
    std::os::unix::fs::symlink(&outside, &apk).unwrap();
    assert!(exec_files::stage_inputs(&root, std::slice::from_ref(&apk), into.path()).is_err(), "a final link");
    std::fs::remove_file(&apk).unwrap();
    std::fs::remove_dir(root.join("build")).unwrap();
    std::os::unix::fs::symlink(dir.path(), root.join("build")).unwrap();
    std::fs::write(dir.path().join("app.apk"), b"elsewhere").unwrap();
    assert!(exec_files::stage_inputs(&root, &[apk], into.path()).is_err(), "a folder link");
    // Write: the approved folder is now a link out, and so is a file.
    let pulled = tempfile::tempdir().unwrap();
    std::fs::create_dir(pulled.path().join("x")).unwrap();
    std::fs::write(pulled.path().join("x/secret.txt"), b"overwritten").unwrap();
    std::os::unix::fs::symlink(dir.path(), root.join("x")).unwrap();
    assert!(exec_files::place_pulled(&root, pulled.path(), &root, 1).is_err());
    assert_eq!(std::fs::read(&outside).unwrap(), b"x", "the file outside is untouched");
    std::fs::remove_file(root.join("x")).unwrap();
    std::fs::create_dir(root.join("x")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("x/secret.txt")).unwrap();
    assert!(exec_files::place_pulled(&root, pulled.path(), &root, 1).is_err());
    assert_eq!(std::fs::read(&outside).unwrap(), b"x", "never written through a link");
    // A pulled tree with git's folder inside it is not placed.
    let git = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(git.path().join("repo/.Git")).unwrap();
    std::fs::write(git.path().join("repo/.Git/config"), b"[core]").unwrap();
    assert!(exec_files::place_pulled(&root, git.path(), &root, 1).is_err());
    assert!(!root.join("repo/.Git/config").exists());
}

#[test]
fn a_pull_says_how_many_files_it_placed() {
    let placed = output((runner::Bounded { status: Some(0), ..Default::default() }, Some(2)));
    assert_eq!(placed.stderr, "[oximux: 2 files placed in the worktree]");
}

/// adb joins `emu`'s words into one console line: a line break in one would
/// start a console command the allowlist never saw.
#[test]
fn no_word_carries_a_control_character() {
    let (_dir, w) = worktree();
    for (tool, words) in [(Tool::Adb, vec!["emu", "ping", "\nkill"]), (Tool::Adb, vec!["shell", "ls\rreboot"]), (Tool::Simctl, vec!["ui", "appearance", "dark\t"])] {
        let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        assert!(matches!(check(tool, &argv, &w), Err(SimErrorWire::BadInput(_))), "{words:?}");
    }
}

/// A file with a second name (a hard link from outside) or a named pipe is
/// not read or written, and a pipe never hangs the transfer.
#[cfg(target_os = "macos")]
#[test]
fn only_plain_files_are_moved() {
    let (dir, w) = worktree();
    let root = std::fs::canonicalize(&w).unwrap();
    let into = tempfile::tempdir().unwrap();
    std::fs::hard_link(dir.path().join("secret.txt"), root.join("linked.txt")).unwrap();
    assert!(exec_files::stage_inputs(&root, &[root.join("linked.txt")], into.path()).is_err(), "a hard link");
    let fifo = root.join("pipe");
    let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: a NUL-terminated path in a folder this test owns.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    assert!(exec_files::stage_inputs(&root, std::slice::from_ref(&fifo), into.path()).is_err(), "a pipe, without hanging");
    let pulled = tempfile::tempdir().unwrap();
    std::fs::write(pulled.path().join("linked.txt"), b"new").unwrap();
    assert!(exec_files::place_pulled(&root, pulled.path(), &root, 1).is_err(), "never written through a second name");
    assert_eq!(std::fs::read(dir.path().join("secret.txt")).unwrap(), b"x");
    std::fs::remove_file(pulled.path().join("linked.txt")).unwrap();
    std::fs::write(pulled.path().join("pipe"), b"new").unwrap();
    assert!(exec_files::place_pulled(&root, pulled.path(), &root, 1).is_err(), "nor into a pipe");
}

/// A file a pull overwrites loses its execute bit too.
#[cfg(target_os = "macos")]
#[test]
fn an_overwritten_file_is_never_executable() {
    use std::os::unix::fs::PermissionsExt as _;
    let (_dir, w) = worktree();
    let root = std::fs::canonicalize(&w).unwrap();
    std::fs::write(root.join("run.sh"), b"old").unwrap();
    std::fs::set_permissions(root.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let pulled = tempfile::tempdir().unwrap();
    std::fs::write(pulled.path().join("run.sh"), b"new").unwrap();
    assert_eq!(exec_files::place_pulled(&root, pulled.path(), &root, 1).unwrap(), 1);
    assert_eq!(std::fs::read(root.join("run.sh")).unwrap(), b"new");
    assert_eq!(std::fs::metadata(root.join("run.sh")).unwrap().permissions().mode() & 0o777, 0o644);
}

