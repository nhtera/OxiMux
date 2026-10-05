use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

const PARENT: &str = "11111111-2222-3333-4444-555555555555";
const CHILD: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// A fresh dir per test. pid + counter, not a timestamp: parallel tests that
/// share a name delete each other's trees.
fn temp_dir(tag: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "oximux-history-test-{}-{}-{tag}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn id(s: &str) -> TabId {
    TabId::parse(s).unwrap()
}

#[test]
fn only_a_lowercase_uuid_is_a_tab_id() {
    assert!(TabId::parse(PARENT).is_some());
    for bad in [
        "",
        "11111111-2222-3333-4444-55555555555",   // short
        "11111111-2222-3333-4444-5555555555555", // long
        "11111111-2222-3333-4444-55555555555A",  // uppercase
        "11111111x2222-3333-4444-555555555555",  // separator
        "11111111-2222-3333-4444-55555555555g",  // non-hex
        "../11111-2222-3333-4444-555555555555",  // traversal, right length
        "11111111-2222-3333-4444-55555555555\n", // newline
        "tab-1",
    ] {
        assert!(TabId::parse(bad).is_none(), "accepted {bad:?}");
    }
}

#[test]
fn tab_files_stay_in_the_dir() {
    let dir = Path::new("/data/shell-history");
    assert_eq!(
        tab_file(dir, &id(PARENT), HistoryShell::Zsh),
        dir.join(format!("{PARENT}.zsh_history"))
    );
    assert_eq!(
        tab_file(dir, &id(PARENT), HistoryShell::Bash),
        dir.join(format!("{PARENT}.bash_history"))
    );
}

#[test]
fn inherit_copies_each_shell_file_the_parent_has() {
    let dir = temp_dir("inherit");
    fs::write(tab_file(&dir, &id(PARENT), HistoryShell::Zsh), ": 1:0;echo zsh\n").unwrap();
    fs::write(tab_file(&dir, &id(PARENT), HistoryShell::Bash), "echo bash\n").unwrap();
    inherit(&dir, &id(PARENT), &id(CHILD)).unwrap();
    assert_eq!(
        fs::read_to_string(tab_file(&dir, &id(CHILD), HistoryShell::Zsh)).unwrap(),
        ": 1:0;echo zsh\n"
    );
    assert_eq!(
        fs::read_to_string(tab_file(&dir, &id(CHILD), HistoryShell::Bash)).unwrap(),
        "echo bash\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(tab_file(&dir, &id(CHILD), HistoryShell::Zsh)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn only_a_terminal_with_a_file_has_history() {
    let dir = temp_dir("has-history");
    assert!(!has_history(&dir, &id(PARENT)));
    fs::write(tab_file(&dir, &id(PARENT), HistoryShell::Bash), "ls\n").unwrap();
    assert!(has_history(&dir, &id(PARENT)));
    // fish: only through a pointer to a file that is really there.
    let fish = dir.join(format!("{}_history", id(CHILD).fish_session()));
    fs::write(fish_pointer(&dir, &id(CHILD)), format!("{}\n", fish.display())).unwrap();
    assert!(!has_history(&dir, &id(CHILD)));
    fs::write(&fish, "- cmd: ls\n").unwrap();
    assert!(has_history(&dir, &id(CHILD)));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn inherit_without_parent_history_creates_nothing() {
    let dir = temp_dir("inherit-none");
    inherit(&dir, &id(PARENT), &id(CHILD)).unwrap();
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn inherit_never_overwrites_an_existing_child() {
    let dir = temp_dir("inherit-existing");
    fs::write(tab_file(&dir, &id(PARENT), HistoryShell::Zsh), "parent\n").unwrap();
    fs::write(tab_file(&dir, &id(CHILD), HistoryShell::Zsh), "child\n").unwrap();
    inherit(&dir, &id(PARENT), &id(CHILD)).unwrap();
    assert_eq!(fs::read_to_string(tab_file(&dir, &id(CHILD), HistoryShell::Zsh)).unwrap(), "child\n");
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn inherit_does_not_write_through_a_planted_symlink() {
    let dir = temp_dir("inherit-symlink");
    let victim = dir.join("victim");
    fs::write(&victim, "untouched\n").unwrap();
    fs::write(tab_file(&dir, &id(PARENT), HistoryShell::Zsh), "parent\n").unwrap();
    std::os::unix::fs::symlink(&victim, tab_file(&dir, &id(CHILD), HistoryShell::Zsh)).unwrap();
    inherit(&dir, &id(PARENT), &id(CHILD)).unwrap();
    assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched\n");
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn inherit_skips_a_parent_that_is_a_symlink() {
    let dir = temp_dir("inherit-parent-symlink");
    let secret = dir.join("secret");
    fs::write(&secret, "not history\n").unwrap();
    std::os::unix::fs::symlink(&secret, tab_file(&dir, &id(PARENT), HistoryShell::Zsh)).unwrap();
    inherit(&dir, &id(PARENT), &id(CHILD)).unwrap();
    assert!(!tab_file(&dir, &id(CHILD), HistoryShell::Zsh).exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn inherit_follows_the_fish_pointer_and_records_the_childs() {
    let dir = temp_dir("inherit-fish");
    let fish_dir = temp_dir("inherit-fish-data");
    let parent_file = fish_dir.join(format!("{}_history", id(PARENT).fish_session()));
    fs::write(&parent_file, "- cmd: echo fish\n").unwrap();
    fs::write(fish_pointer(&dir, &id(PARENT)), format!("{}\n", parent_file.display())).unwrap();

    inherit(&dir, &id(PARENT), &id(CHILD)).unwrap();

    let child_file = fish_dir.join("oximux_aaaaaaaa_bbbb_cccc_dddd_eeeeeeeeeeee_history");
    assert_eq!(fs::read_to_string(&child_file).unwrap(), "- cmd: echo fish\n");
    assert_eq!(fish_target(&dir, &id(CHILD)), Some(child_file));
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&fish_dir);
}

#[test]
fn a_fish_pointer_to_any_other_file_is_ignored() {
    let dir = temp_dir("fish-pointer");
    let fish_dir = temp_dir("fish-pointer-data");
    let other = fish_dir.join("fish_history"); // the user's own history
    fs::write(&other, "- cmd: keep me\n").unwrap();
    for bad in [other.display().to_string(), format!("oximux_{}_history", PARENT.replace('-', "_"))] {
        fs::write(fish_pointer(&dir, &id(PARENT)), format!("{bad}\n")).unwrap();
        assert_eq!(fish_target(&dir, &id(PARENT)), None, "{bad}");
        forget(&dir, &id(PARENT)).unwrap();
    }
    assert!(other.exists(), "forget must never reach a file the pointer does not own");
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&fish_dir);
}

#[cfg(unix)]
#[test]
fn a_fish_pointer_to_a_symlink_is_ignored() {
    let dir = temp_dir("fish-symlink");
    let fish_dir = temp_dir("fish-symlink-data");
    let victim = fish_dir.join("victim");
    fs::write(&victim, "keep\n").unwrap();
    let link = fish_dir.join(format!("{}_history", id(PARENT).fish_session()));
    std::os::unix::fs::symlink(&victim, &link).unwrap();
    fs::write(fish_pointer(&dir, &id(PARENT)), format!("{}\n", link.display())).unwrap();
    assert_eq!(fish_target(&dir, &id(PARENT)), None);
    forget(&dir, &id(PARENT)).unwrap();
    assert!(victim.exists());
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&fish_dir);
}

#[test]
fn forget_removes_every_file_of_that_terminal_only() {
    let dir = temp_dir("forget");
    let fish_dir = temp_dir("forget-fish");
    let fish_file = fish_dir.join(format!("{}_history", id(PARENT).fish_session()));
    fs::write(&fish_file, "- cmd: x\n").unwrap();
    let mine = [
        format!("{PARENT}.zsh_history"),
        format!("{PARENT}.zsh_history.new"),
        format!("{PARENT}.zsh_history.LOCK"),
        format!("{PARENT}.bash_history"),
        format!("{PARENT}.bash_history.new"),
    ];
    for name in &mine {
        fs::write(dir.join(name), "x").unwrap();
    }
    fs::write(fish_pointer(&dir, &id(PARENT)), format!("{}\n", fish_file.display())).unwrap();
    let theirs = dir.join(format!("{CHILD}.zsh_history"));
    fs::write(&theirs, "x").unwrap();

    forget(&dir, &id(PARENT)).unwrap();

    for name in &mine {
        assert!(!dir.join(name).exists(), "{name} survived");
    }
    assert!(!fish_pointer(&dir, &id(PARENT)).exists());
    assert!(!fish_file.exists());
    assert!(theirs.exists(), "another terminal's history must survive");
    // Forgetting twice (the delayed re-forget after a close) is fine.
    forget(&dir, &id(PARENT)).unwrap();
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&fish_dir);
}

#[test]
fn gc_keeps_live_and_recent_history_and_drops_the_rest() {
    let dir = temp_dir("gc");
    let live = "22222222-2222-3333-4444-555555555555";
    for tab in [PARENT, CHILD, live] {
        fs::write(dir.join(format!("{tab}.zsh_history")), "x").unwrap();
    }
    fs::write(dir.join("notes.txt"), "x").unwrap();
    fs::write(dir.join(format!("{PARENT}.something_else")), "x").unwrap();
    let referenced: HashSet<TabId> = [id(live)].into_iter().collect();

    // Everything is fresh: nothing goes.
    assert_eq!(gc(&dir, &referenced, 30 * DAY, SystemTime::now()).unwrap(), 0);
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 5);

    // A month on, the two unreferenced terminals go; the live one and every
    // file the module did not write stay.
    let later = SystemTime::now() + 31 * DAY;
    assert_eq!(gc(&dir, &referenced, 30 * DAY, later).unwrap(), 2);
    assert!(!dir.join(format!("{PARENT}.zsh_history")).exists());
    assert!(!dir.join(format!("{CHILD}.zsh_history")).exists());
    assert!(dir.join(format!("{live}.zsh_history")).exists());
    assert!(dir.join("notes.txt").exists());
    assert!(dir.join(format!("{PARENT}.something_else")).exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn only_module_file_names_map_to_a_terminal() {
    assert_eq!(owning_tab(&format!("{PARENT}.zsh_history")), Some(id(PARENT)));
    assert_eq!(owning_tab(&format!("{PARENT}.bash_history.new")), Some(id(PARENT)));
    assert_eq!(owning_tab(&format!("{PARENT}.zsh_history.LOCK")), Some(id(PARENT)));
    assert_eq!(owning_tab(&format!("{PARENT}.fish_path")), Some(id(PARENT)));
    assert_eq!(owning_tab(&format!("{PARENT}.zsh_historyX")), None);
    assert_eq!(owning_tab(&format!("{PARENT}.txt")), None);
    assert_eq!(owning_tab("short.zsh_history"), None);
    assert_eq!(owning_tab("Ünïcode-name-that-is-long-enough-to-split.zsh_history"), None);
}

#[test]
fn every_block_validates_the_id_and_honours_the_opt_out() {
    let uuid_re = "^[0-9a-f]{8}(-[0-9a-f]{4}){3}-[0-9a-f]{12}$";
    for (name, block) in [
        ("zsh", scripts::ZSH_BLOCK),
        ("bash", scripts::BASH_BLOCK),
        ("fish", scripts::FISH_BLOCK),
    ] {
        assert!(block.contains(uuid_re), "{name}: no UUID check");
        assert!(block.contains(OPT_OUT_ENV), "{name}: no opt-out");
        assert!(block.contains(HISTORY_DIR_ENV), "{name}: no dir gate");
        assert!(block.contains(TAB_ID_ENV), "{name}: no tab id");
    }
}

#[test]
fn the_shell_blocks_stand_down_where_history_is_shared_or_off() {
    let zsh = scripts::ZSH_BLOCK;
    assert!(zsh.contains("! -o share_history"));
    assert!(zsh.contains("${SAVEHIST:-0} -gt 0"));
    assert!(zsh.contains("${HISTFILE:-} != \"$__oximux_tab_hist\""), "incognito pauses the tee");
    let bash = scripts::BASH_BLOCK;
    assert!(bash.contains("history +-[a-z]*[nr]"), "live sharing via PROMPT_COMMAND");
    assert!(bash.contains("shopt -oq history"), "set +o history stops the flush");
    assert!(bash.contains("-z \"${MSYSTEM:-}\""), "Git Bash stays on shared history");
    let fish = scripts::FISH_BLOCK;
    assert!(fish.contains("not set -q fish_private_mode"));
    assert!(fish.contains("([4-9]|[1-9][0-9])\\."), "fish 3.x lacks `history append`");
}

#[test]
fn a_fish_copy_backdates_entries_from_the_last_seconds() {
    let dir = temp_dir("fish-backdate");
    let file = dir.join("h");
    fs::write(&file, "- cmd: old\n  when: 1000\n- cmd: fresh\n  when: 2000\n  paths:\n    - x\n").unwrap();
    let now = std::time::UNIX_EPOCH + Duration::from_secs(2001);
    backdate_recent_fish_items(&file, now).unwrap();
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "- cmd: old\n  when: 1000\n- cmd: fresh\n  when: 1999\n  paths:\n    - x\n"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn the_first_close_pass_keeps_the_fish_pointer_for_the_second() {
    let dir = temp_dir("forget-two-pass");
    let fish_dir = temp_dir("forget-two-pass-fish");
    let fish_file = fish_dir.join(format!("{}_history", id(PARENT).fish_session()));
    fs::write(&fish_file, "- cmd: x\n").unwrap();
    fs::write(fish_pointer(&dir, &id(PARENT)), format!("{}\n", fish_file.display())).unwrap();
    forget_keeping_fish_pointer(&dir, &id(PARENT)).unwrap();
    assert!(!fish_file.exists());
    // fish rewrites its history as it exits...
    fs::write(&fish_file, "- cmd: x\n").unwrap();
    // ...and the second pass still finds it.
    forget(&dir, &id(PARENT)).unwrap();
    assert!(!fish_file.exists());
    assert!(!fish_pointer(&dir, &id(PARENT)).exists());
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&fish_dir);
}

#[cfg(unix)]
#[test]
fn a_fish_file_that_cannot_be_deleted_keeps_its_pointer() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir("forget-keep-pointer");
    let fish_dir = temp_dir("forget-keep-pointer-fish");
    let fish_file = fish_dir.join(format!("{}_history", id(PARENT).fish_session()));
    fs::write(&fish_file, "- cmd: x\n").unwrap();
    fs::write(fish_pointer(&dir, &id(PARENT)), format!("{}\n", fish_file.display())).unwrap();
    fs::set_permissions(&fish_dir, fs::Permissions::from_mode(0o500)).unwrap();
    assert!(forget(&dir, &id(PARENT)).is_err());
    assert!(fish_pointer(&dir, &id(PARENT)).exists(), "a later pass must still find the file");
    fs::set_permissions(&fish_dir, fs::Permissions::from_mode(0o700)).unwrap();
    forget(&dir, &id(PARENT)).unwrap();
    assert!(!fish_file.exists() && !fish_pointer(&dir, &id(PARENT)).exists());
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&fish_dir);
}
