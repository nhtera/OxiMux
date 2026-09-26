use super::*;

type Reg = Registry<&'static str>;

fn wt(name: &str) -> WorktreeKey {
    WorktreeKey(PathBuf::from(format!("/w/{name}")))
}

fn dev(u: &str) -> DeviceId {
    DeviceId(u.into())
}

/// Effect kinds, for compact assertions.
fn kinds(effects: &[Effect<&'static str>]) -> Vec<String> {
    effects
        .iter()
        .map(|e| match e {
            Effect::Boot { udid, .. } => format!("boot {udid}"),
            Effect::StartSession { udid, .. } => format!("start {udid}"),
            Effect::StopSession { udid, session } => format!("stop {udid} {session}"),
            Effect::Pause(s) => format!("pause {s}"),
            Effect::Resume(s) => format!("resume {s}"),
            Effect::ShutdownDevice { udid } => format!("shutdown {udid}"),
            Effect::Persist => "persist".into(),
        })
        .collect()
}

fn start_gen(effects: &[Effect<&'static str>]) -> Generation {
    effects.iter().find_map(|e| match e {
        Effect::StartSession { generation, .. } | Effect::Boot { generation, .. } => Some(*generation),
        _ => None,
    }).expect("a start or boot effect")
}

/// Attach to a booted device and bring its session live.
fn live(reg: &mut Reg, w: &WorktreeKey, u: &DeviceId, session: &'static str, now: Instant) -> Generation {
    let fx = reg.attach(w.clone(), u.clone(), true, now);
    let g = start_gen(&fx);
    reg.set_visible(w, true, now);
    reg.session_started(u, g, Ok(session));
    assert_eq!(reg.phase(u), Phase::Live { generation: g });
    g
}

#[test]
fn a_user_booted_device_is_never_shut_down() {
    let now = Instant::now();
    let mut reg = Reg::default();
    let (w, u) = (wt("a"), dev("U"));
    live(&mut reg, &w, &u, "s1", now);
    assert!(!reg.is_owned(&u));
    let fx = reg.detach(&w, now);
    assert_eq!(kinds(&fx), ["stop U s1", "persist"]);
    assert!(reg.tick(now + IDLE_SHUTDOWN * 10).is_empty());
    let (sessions, owned) = reg.quit();
    assert!(sessions.is_empty() && owned.is_empty());
}

#[test]
fn an_owned_device_shuts_down_ten_minutes_after_its_last_detach() {
    let now = Instant::now();
    let mut reg = Reg::default();
    let (w, u) = (wt("a"), dev("U"));
    let fx = reg.attach(w.clone(), u.clone(), false, now);
    assert_eq!(kinds(&fx), ["boot U", "persist"]);
    assert!(reg.is_owned(&u));
    let g = start_gen(&fx);
    let fx = reg.boot_finished(&u, g, BootResult::Booted);
    let g2 = start_gen(&fx);
    reg.session_started(&u, g2, Ok("s1"));
    reg.detach(&w, now);
    assert!(reg.tick(now + IDLE_SHUTDOWN - Duration::from_secs(1)).is_empty());
    assert_eq!(kinds(&reg.tick(now + IDLE_SHUTDOWN)), ["shutdown U", "persist"]);
    assert!(!reg.is_owned(&u));
}

#[test]
fn reattaching_cancels_the_idle_shutdown() {
    let now = Instant::now();
    let mut reg = Reg::default();
    let (w, u) = (wt("a"), dev("U"));
    reg.attach(w.clone(), u.clone(), false, now);
    reg.detach(&w, now);
    reg.attach(w.clone(), u.clone(), true, now + Duration::from_secs(60));
    assert!(reg.tick(now + IDLE_SHUTDOWN * 2).is_empty());
}

#[test]
fn quit_stops_sessions_and_names_only_owned_devices() {
    let now = Instant::now();
    let mut reg = Reg::default();
    live(&mut reg, &wt("a"), &dev("USER"), "s1", now);
    let fx = reg.attach(wt("b"), dev("OURS"), false, now);
    let fx = reg.boot_finished(&dev("OURS"), start_gen(&fx), BootResult::Booted);
    reg.session_started(&dev("OURS"), start_gen(&fx), Ok("s2"));
    let (mut sessions, owned) = reg.quit();
    sessions.sort();
    assert_eq!(sessions, ["s1", "s2"]);
    assert_eq!(owned, [dev("OURS")]);
}

#[test]
fn a_boot_cancelled_by_detach_is_ignored_and_the_device_stays_owned() {
    let now = Instant::now();
    let mut reg = Reg::default();
    let (w, u) = (wt("a"), dev("U"));
    let fx = reg.attach(w.clone(), u.clone(), false, now);
    let Effect::Boot { cancel, generation, .. } = &fx[0] else { panic!("{fx:?}") };
    reg.detach(&w, now);
    assert!(cancel.load(Ordering::SeqCst), "detach cancels the boot");
    assert!(reg.boot_finished(&u, *generation, BootResult::Booted).is_empty(), "stale boot result");
    assert_eq!(reg.phase(&u), Phase::Idle);
    // Booted by us regardless, so the idle rule still shuts it down.
    assert_eq!(kinds(&reg.tick(now + IDLE_SHUTDOWN)), ["shutdown U", "persist"]);
}

#[test]
fn a_late_session_from_an_old_generation_is_stopped_not_installed() {
    let now = Instant::now();
    let mut reg = Reg::default();
    let (w, u, v) = (wt("a"), dev("U"), dev("V"));
    let old = start_gen(&reg.attach(w.clone(), u.clone(), true, now));
    // Switch before the first helper finished its handshake.
    reg.attach(w.clone(), v.clone(), true, now);
    let fx = reg.session_started(&u, old, Ok("late"));
    assert_eq!(kinds(&fx), ["stop U late"]);
    assert!(reg.session(&u).is_none());
    // Same when the generation is superseded on the same device.
    let first = start_gen(&reg.attach(wt("b"), u.clone(), true, now));
    reg.detach(&wt("b"), now);
    let second = start_gen(&reg.attach(wt("b"), u.clone(), true, now));
    assert_ne!(first, second);
    assert_eq!(kinds(&reg.session_started(&u, first, Ok("stale"))), ["stop U stale"]);
    reg.set_visible(&wt("b"), true, now);
    assert!(reg.session_started(&u, second, Ok("fresh")).is_empty());
    assert_eq!(reg.session(&u), Some(&"fresh"));
}

#[test]
fn two_worktrees_share_one_session_and_refcount_visibility() {
    let now = Instant::now();
    let mut reg = Reg::default();
    let u = dev("U");
    live(&mut reg, &wt("a"), &u, "s", now);
    // The second worktree joins the live session: no new start.
    let fx = reg.attach(wt("b"), u.clone(), true, now);
    assert_eq!(kinds(&fx), ["persist"]);
    reg.set_visible(&wt("b"), true, now);
    // Hiding one viewer must not pause the other's stream.
    assert!(reg.set_visible(&wt("a"), false, now).is_empty());
    assert_eq!(kinds(&reg.set_visible(&wt("b"), false, now)), ["pause s"]);
    assert_eq!(kinds(&reg.set_visible(&wt("a"), true, now)), ["resume s"]);
    // Detaching one keeps the session; detaching the last stops it.
    assert_eq!(kinds(&reg.detach(&wt("a"), now)), ["pause s", "persist"]);
    assert_eq!(kinds(&reg.detach(&wt("b"), now)), ["stop U s", "persist"]);
}

#[test]
fn a_session_that_starts_while_hidden_is_paused_at_once() {
    let now = Instant::now();
    let mut reg = Reg::default();
    let (w, u) = (wt("a"), dev("U"));
    let g = start_gen(&reg.attach(w, u.clone(), true, now));
    assert_eq!(kinds(&reg.session_started(&u, g, Ok("s"))), ["pause s"]);
}

#[test]
fn switching_device_detaches_the_old_one() {
    let now = Instant::now();
    let mut reg = Reg::default();
    let w = wt("a");
    live(&mut reg, &w, &dev("U"), "s", now);
    let fx = reg.attach(w.clone(), dev("V"), true, now);
    assert_eq!(kinds(&fx), ["stop U s", "persist", "start V", "persist"]);
    assert_eq!(reg.device_for(&w), Some(&dev("V")));
    // Re-attaching the same device is a no-op.
    assert!(reg.attach(w, dev("V"), true, now).is_empty());
}

#[test]
fn a_helper_exit_restarts_once_then_shows_disconnected() {
    let now = Instant::now();
    let mut reg = Reg::default();
    let (w, u) = (wt("a"), dev("U"));
    let g = live(&mut reg, &w, &u, "s1", now);
    let fx = reg.session_exited(&u, g, true, "exited".into());
    assert_eq!(kinds(&fx), ["stop U s1", "start U"]);
    let g2 = start_gen(&fx);
    reg.session_started(&u, g2, Ok("s2"));
    let fx = reg.session_exited(&u, g2, true, "exited again".into());
    assert_eq!(kinds(&fx), ["stop U s2"]);
    assert_eq!(reg.phase(&u), Phase::Disconnected { reason: "exited again".into() });
    // Reconnect is the user's move.
    let fx = reg.reconnect(&u, true);
    assert_eq!(kinds(&fx), ["start U"]);
    reg.session_started(&u, start_gen(&fx), Ok("s3"));
    // Restarting a live session (Xcode changed) stops it first.
    assert_eq!(kinds(&reg.reconnect(&u, true)), ["stop U s3", "start U"]);
}

#[test]
fn a_stale_exit_is_ignored() {
    let now = Instant::now();
    let mut reg = Reg::default();
    let (w, u) = (wt("a"), dev("U"));
    let g = live(&mut reg, &w, &u, "s", now);
    assert!(reg.session_exited(&u, g + 100, true, "old".into()).is_empty());
    assert!(matches!(reg.phase(&u), Phase::Live { .. }));
}

#[test]
fn a_device_shut_down_elsewhere_disconnects_and_is_no_longer_ours() {
    let now = Instant::now();
    let mut reg = Reg::default();
    let (w, u) = (wt("a"), dev("U"));
    let fx = reg.attach(w.clone(), u.clone(), false, now);
    let fx = reg.boot_finished(&u, start_gen(&fx), BootResult::Booted);
    reg.session_started(&u, start_gen(&fx), Ok("s"));
    let fx = reg.device_shutdown(&u);
    assert_eq!(kinds(&fx), ["stop U s", "persist"]);
    assert!(matches!(reg.phase(&u), Phase::Disconnected { .. }));
    assert!(!reg.is_owned(&u));
    // A helper exit racing the shutdown is stale now.
    assert!(reg.session_exited(&u, 1, false, "x".into()).is_empty());
}

#[test]
fn a_failed_boot_or_start_shows_failed() {
    let now = Instant::now();
    let mut reg = Reg::default();
    let fx = reg.attach(wt("a"), dev("U"), false, now);
    reg.boot_finished(&dev("U"), start_gen(&fx), BootResult::Failed("no runtime".into()));
    assert_eq!(reg.phase(&dev("U")), Phase::Failed { error: "no runtime".into() });
    let g = start_gen(&reg.attach(wt("b"), dev("V"), true, now));
    reg.session_started(&dev("V"), g, Err("framework".into()));
    assert_eq!(reg.phase(&dev("V")), Phase::Failed { error: "framework".into() });
}

#[test]
fn snapshot_round_trips_intent_and_ownership() {
    let now = Instant::now();
    let mut reg = Reg::default();
    reg.attach(wt("a"), dev("OURS"), false, now);
    live(&mut reg, &wt("b"), &dev("USER"), "s", now);
    let snap = reg.snapshot();
    assert_eq!(snap.owned_boots, [dev("OURS")]);
    assert_eq!(snap.attachments.len(), 2);
    let json = serde_json::to_string(&snap).unwrap();
    let back: Snapshot = serde_json::from_str(&json).unwrap();
    let restored = Reg::restore(back, now);
    assert_eq!(restored.device_for(&wt("a")), Some(&dev("OURS")));
    assert!(restored.is_owned(&dev("OURS")));
    assert_eq!(restored.phase(&dev("OURS")), Phase::Idle, "no process is restored");
}

#[test]
fn an_owned_device_restored_unattached_resumes_its_idle_clock() {
    let now = Instant::now();
    let snap = Snapshot { attachments: vec![], owned_boots: vec![dev("OURS")] };
    let mut reg = Reg::restore(snap, now);
    assert_eq!(kinds(&reg.tick(now + IDLE_SHUTDOWN)), ["shutdown OURS", "persist"]);
}

#[test]
fn worktree_keys_canonicalize_symlinks_and_survive_deletion() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    #[cfg(unix)]
    {
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(WorktreeKey::from_path(&link), WorktreeKey::from_path(&real));
    }
    let gone = dir.path().join("deleted");
    assert_eq!(WorktreeKey::from_path(&gone).path(), gone.as_path());
}

#[test]
fn auto_pick_prefers_a_booted_iphone_then_the_setting_then_the_newest_iphone() {
    let d = |udid: &str, kind, os: &str, booted: bool| DeviceInfo {
        udid: dev(udid),
        name: udid.into(),
        runtime: String::new(),
        os_version: os.into(),
        state: if booted { DeviceState::Booted } else { DeviceState::Shutdown },
        kind,
        is_available: true,
    };
    let list = vec![
        d("PAD", DeviceKind::Tablet, "26.3", true),
        d("OLD", DeviceKind::Phone, "18.6", false),
        d("NEW", DeviceKind::Phone, "26.3", false),
        d("WATCH", DeviceKind::Other, "26.0", true),
    ];
    let pick = |list: &[DeviceInfo], pref: Option<&str>| {
        auto_pick(list, pref.map(dev).as_ref()).map(|(d, b)| (d.udid.0.clone(), b))
    };
    assert_eq!(pick(&list, None), Some(("NEW".into(), false)));
    assert_eq!(pick(&list, Some("OLD")), Some(("OLD".into(), false)));
    assert_eq!(pick(&list, Some("PAD")), Some(("PAD".into(), true)));
    let mut with_booted = list.clone();
    with_booted.push(d("RUNNING", DeviceKind::Phone, "18.6", true));
    assert_eq!(pick(&with_booted, Some("PAD")), Some(("RUNNING".into(), true)));
    assert_eq!(pick(&[], None), None);
}

fn booted(ids: &[&str]) -> BTreeSet<DeviceId> {
    ids.iter().map(|s| dev(s)).collect()
}

/// Review C1: quit hands owned devices to the shutdown script and forgets
/// them, so the snapshot saved after quit claims nothing.
#[test]
fn quit_clears_ownership_before_the_snapshot_is_saved() {
    let now = Instant::now();
    let mut reg = Reg::default();
    let fx = reg.attach(wt("a"), dev("OURS"), false, now);
    reg.boot_finished(&dev("OURS"), start_gen(&fx), BootResult::Booted);
    let (_, owned) = reg.quit();
    assert_eq!(owned, [dev("OURS")]);
    assert!(reg.snapshot().owned_boots.is_empty());
}

/// Review C1: a restored "owned" device that is not booted any more (our
/// quit script, or the user, shut it down) is not ours when the user boots it
/// again later.
#[test]
fn a_restored_owned_device_seen_shut_down_is_no_longer_ours() {
    let now = Instant::now();
    let snap = Snapshot { attachments: vec![(wt("a"), dev("U"))], owned_boots: vec![dev("U")] };
    let mut reg = Reg::restore(snap, now);
    assert!(reg.is_owned(&dev("U")));
    assert_eq!(kinds(&reg.reconcile_booted(&booted(&[]))), ["persist"]);
    assert!(!reg.is_owned(&dev("U")));
    // The user boots it (Xcode Cmd+R); nothing we do shuts it down.
    assert!(reg.reconcile_booted(&booted(&["U"])).is_empty());
    reg.detach(&wt("a"), now);
    assert!(reg.tick(now + IDLE_SHUTDOWN * 3).is_empty());
    assert!(reg.quit().1.is_empty());
}

#[test]
fn reconcile_leaves_a_booting_device_alone_and_disconnects_a_dead_session() {
    let now = Instant::now();
    let mut reg = Reg::default();
    reg.attach(wt("a"), dev("B"), false, now); // Booting, not yet booted
    live(&mut reg, &wt("b"), &dev("L"), "s", now);
    let fx = reg.reconcile_booted(&booted(&[]));
    assert_eq!(kinds(&fx), ["stop L s"]);
    assert!(reg.is_owned(&dev("B")), "mid-boot devices keep their ownership");
    assert!(matches!(reg.phase(&dev("L")), Phase::Disconnected { .. }));
}

/// Review H1: only a boot we performed makes a device ours — even when the
/// result is stale.
#[test]
fn an_already_booted_or_failed_boot_is_never_ours() {
    let now = Instant::now();
    let mut reg = Reg::default();
    let fx = reg.attach(wt("a"), dev("U"), false, now);
    let fx = reg.boot_finished(&dev("U"), start_gen(&fx), BootResult::AlreadyBooted);
    assert_eq!(kinds(&fx), ["persist", "start U"]);
    assert!(!reg.is_owned(&dev("U")));

    let fx = reg.attach(wt("b"), dev("V"), false, now);
    let g = start_gen(&fx);
    reg.detach(&wt("b"), now);
    // Stale and already booted: still not ours.
    assert_eq!(kinds(&reg.boot_finished(&dev("V"), g, BootResult::AlreadyBooted)), ["persist"]);
    assert!(!reg.is_owned(&dev("V")));
    assert!(reg.tick(now + IDLE_SHUTDOWN).is_empty());

    let fx = reg.attach(wt("c"), dev("W"), false, now);
    reg.boot_finished(&dev("W"), start_gen(&fx), BootResult::Failed("busy".into()));
    assert!(!reg.is_owned(&dev("W")));
    // A cancelled boot stays ours: it may still complete.
    let fx = reg.attach(wt("d"), dev("X"), false, now);
    reg.boot_finished(&dev("X"), start_gen(&fx), BootResult::Cancelled);
    assert!(reg.is_owned(&dev("X")));
}

/// Review M2: re-attaching a restored (Idle) attachment starts it.
#[test]
fn attaching_the_restored_device_again_starts_it() {
    let now = Instant::now();
    let snap = Snapshot { attachments: vec![(wt("a"), dev("U"))], owned_boots: vec![] };
    let mut reg = Reg::restore(snap, now);
    assert_eq!(kinds(&reg.attach(wt("a"), dev("U"), true, now)), ["start U"]);
    // While it is starting, attaching again is a no-op.
    assert!(reg.attach(wt("a"), dev("U"), true, now).is_empty());
}

/// Advisor: a panel shown before its attach lands must stream, not start paused.
#[test]
fn visibility_set_before_attach_carries_over() {
    let now = Instant::now();
    let mut reg = Reg::default();
    let (w, u) = (wt("a"), dev("U"));
    assert!(reg.set_visible(&w, true, now).is_empty());
    let g = start_gen(&reg.attach(w.clone(), u.clone(), true, now));
    assert!(reg.session_started(&u, g, Ok("s")).is_empty(), "not paused");
    // Hidden, detached, re-attached while still hidden: starts paused.
    reg.set_visible(&w, false, now);
    reg.detach(&w, now);
    let g = start_gen(&reg.attach(w, u.clone(), true, now));
    assert_eq!(kinds(&reg.session_started(&u, g, Ok("s2"))), ["pause s2"]);
}

/// Advisor: a watcher poll racing our boot may clear ownership; the boot's
/// own result puts it back, and callers can detect the race by generation.
#[test]
fn a_booted_result_reasserts_ownership_and_bumps_the_generation() {
    let now = Instant::now();
    let mut reg = Reg::default();
    let fx = reg.attach(wt("a"), dev("U"), false, now);
    let before = reg.generation();
    let fx = reg.boot_finished(&dev("U"), start_gen(&fx), BootResult::Booted);
    assert_ne!(reg.generation(), before, "a list read across this must be discarded");
    assert!(reg.is_owned(&dev("U")));
    assert_eq!(kinds(&fx), ["start U"]);
    // Force the race: ownership cleared by a stale reconcile, then re-asserted.
    reg.device_shutdown(&dev("U"));
    let fx = reg.attach(wt("b"), dev("V"), false, now);
    reg.devices.get_mut(&dev("V")).unwrap().owned = false;
    assert_eq!(kinds(&reg.boot_finished(&dev("V"), start_gen(&fx), BootResult::Booted)), ["persist", "start V"]);
    assert!(reg.is_owned(&dev("V")));
}

/// A device hidden for `PARK_AFTER` has its helper stopped but stays
/// attached; showing it again starts a fresh helper, and the stopped one's
/// late exit is ignored.
#[test]
fn a_long_hidden_device_is_parked_and_restarts_when_shown() {
    let now = Instant::now();
    let mut reg = Reg::default();
    let (w, u) = (wt("a"), dev("U"));
    let g = live(&mut reg, &w, &u, "s", now);
    assert_eq!(kinds(&reg.set_visible(&w, false, now)), ["pause s"]);
    assert!(reg.tick(now + PARK_AFTER - Duration::from_secs(1)).is_empty());
    assert_eq!(kinds(&reg.tick(now + PARK_AFTER)), ["stop U s"]);
    assert_eq!(reg.phase(&u), Phase::Parked);
    assert_eq!(reg.device_for(&w), Some(&u), "still attached");
    assert!(reg.session_exited(&u, g, true, "closed".into()).is_empty(), "stale exit");
    let generation = reg.generation();
    let fx = reg.set_visible(&w, true, now + PARK_AFTER * 2);
    assert_eq!(kinds(&fx), ["start U"]);
    assert_ne!(reg.generation(), generation);
    let g2 = start_gen(&fx);
    assert!(reg.session_started(&u, g2, Ok("s2")).is_empty(), "shown, so not paused");
    assert_eq!(reg.phase(&u), Phase::Live { generation: g2 });
}

/// An agent driving a hidden device counts as a viewer: the paused helper
/// resumes (a paused one captures nothing), nothing parks it, and once the
/// agent stops it pauses and parks one period later.
#[test]
fn an_agent_is_a_viewer_while_it_drives() {
    let now = Instant::now();
    let mut reg = Reg::default();
    let (w, u) = (wt("a"), dev("U"));
    live(&mut reg, &w, &u, "s", now);
    assert_eq!(kinds(&reg.set_visible(&w, false, now)), ["pause s"]);
    assert_eq!(kinds(&reg.set_agent_active(&u, true, now)), ["resume s"]);
    assert!(reg.tick(now + PARK_AFTER * 3).is_empty(), "never parked while driven");
    let done = now + PARK_AFTER * 3;
    assert_eq!(kinds(&reg.set_agent_active(&u, false, done)), ["pause s"]);
    assert!(reg.tick(done + PARK_AFTER - Duration::from_secs(1)).is_empty());
    assert_eq!(kinds(&reg.tick(done + PARK_AFTER)), ["stop U s"]);
    // A session that starts while an agent drives is not paused at all.
    let (w2, u2) = (wt("b"), dev("V"));
    let g = start_gen(&reg.attach(w2.clone(), u2.clone(), true, now));
    reg.set_agent_active(&u2, true, now);
    assert!(reg.session_started(&u2, g, Ok("t")).is_empty(), "not paused under an agent");
    // Shown in a panel, an agent changes nothing.
    assert!(reg.set_visible(&w2, true, now).is_empty());
    assert!(reg.set_agent_active(&u2, false, now).is_empty());
}

/// A session that starts hidden gets its parking clock from the first tick.
#[test]
fn a_hidden_start_parks_one_period_after_the_first_tick() {
    let now = Instant::now();
    let mut reg = Reg::default();
    let (w, u) = (wt("a"), dev("U"));
    let g = start_gen(&reg.attach(w.clone(), u.clone(), true, now));
    assert_eq!(kinds(&reg.session_started(&u, g, Ok("s"))), ["pause s"]);
    assert!(reg.tick(now).is_empty(), "starts the clock");
    assert!(reg.tick(now + PARK_AFTER - Duration::from_secs(1)).is_empty());
    assert_eq!(kinds(&reg.tick(now + PARK_AFTER)), ["stop U s"]);
    // Hiding without a visible change never moves the generation.
    let generation = reg.generation();
    reg.set_visible(&w, false, now);
    assert_eq!(reg.generation(), generation);
}

/// A parked device that is shut down externally transitions to Disconnected.
#[test]
fn a_parked_device_shut_down_externally_becomes_disconnected() {
    let now = Instant::now();
    let mut reg = Reg::default();
    let (w, u) = (wt("a"), dev("U"));
    let _g = live(&mut reg, &w, &u, "s", now);
    assert_eq!(kinds(&reg.set_visible(&w, false, now)), ["pause s"]);
    assert_eq!(kinds(&reg.tick(now + PARK_AFTER)), ["stop U s"]);
    assert_eq!(reg.phase(&u), Phase::Parked);
    // Device is shut down externally while parked.
    let fx = reg.device_shutdown(&u);
    assert_eq!(kinds(&fx), Vec::<String>::new());  // No effects (not owned, no session to stop)
    assert_eq!(reg.phase(&u), Phase::Disconnected { reason: "The device shut down.".into() });
    assert_eq!(reg.device_for(&w), Some(&u), "attachment is preserved");
}

/// An owned parked device that is shut down externally triggers persist.
#[test]
fn an_owned_parked_device_shut_down_externally_persists() {
    let now = Instant::now();
    let mut reg = Reg::default();
    let (w, u) = (wt("a"), dev("U"));
    let fx = reg.attach(w.clone(), u.clone(), false, now);
    let g = start_gen(&fx);
    let fx = reg.boot_finished(&u, g, BootResult::Booted);
    let g2 = start_gen(&fx);
    reg.set_visible(&w, true, now);
    reg.session_started(&u, g2, Ok("s"));
    assert_eq!(reg.phase(&u), Phase::Live { generation: g2 });
    assert!(reg.is_owned(&u));
    // Hide and park the device.
    assert_eq!(kinds(&reg.set_visible(&w, false, now)), ["pause s"]);
    assert_eq!(kinds(&reg.tick(now + PARK_AFTER)), ["stop U s"]);
    assert_eq!(reg.phase(&u), Phase::Parked);
    // Owned device shut down externally.
    let fx = reg.device_shutdown(&u);
    assert_eq!(kinds(&fx), ["persist"]);
    assert!(!reg.is_owned(&u));
}

/// Review H1: a shown worktree joining a paused device resumes it, so the
/// device is never parked while someone is looking at it.
#[test]
fn a_shown_worktree_joining_a_paused_device_resumes_it() {
    let now = Instant::now();
    let mut reg = Reg::default();
    let u = dev("U");
    live(&mut reg, &wt("a"), &u, "s", now);
    assert_eq!(kinds(&reg.set_visible(&wt("a"), false, now)), ["pause s"]);
    reg.set_visible(&wt("b"), true, now);
    assert_eq!(kinds(&reg.attach(wt("b"), u.clone(), true, now)), ["resume s", "persist"]);
    assert!(reg.tick(now + PARK_AFTER * 2).is_empty(), "not parked while shown");
}
