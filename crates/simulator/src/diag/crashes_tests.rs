use super::*;
use crate::android::adb::Adb;
use crate::diag::crash_buffer::*;
use crate::runner::{CmdOutput, ScriptedRunner};

const SWIFT_TRAP: &str = include_str!("../../tests/fixtures/crash/swift_trap.ips");
const UNCAUGHT: &str = include_str!("../../tests/fixtures/crash/uncaught_exception.ips");
const LOGCAT: &str = include_str!("../../tests/fixtures/crash/logcat_crash.txt");
const UDID: &str = "E0E0A1DD-2DAC-448E-86D3-07B7F809BCD2";

#[test]
fn a_swift_trap_reads_as_its_signal_and_the_crashing_threads_top_frames() {
    let ips = parse_ips(SWIFT_TRAP).unwrap();
    assert!(ips.is_of(UDID) && ips.is_app("dev.oximux.crashy"));
    assert!(!ips.is_of("81CE1BE8-E38A-4BA8-8AAB-5DACA07576B3"));
    let crash = ips.crash;
    assert_eq!(crash.id, "B8E1C770-FF0E-4937-B31D-3412678C6258");
    assert_eq!(crash.app, "dev.oximux.crashy");
    assert_eq!(crash.exception, "EXC_BREAKPOINT (SIGTRAP)");
    assert_eq!(crash.reason, "Trace/BPT trap: 5");
    assert_eq!(crash.pid, Some(61726));
    assert_eq!(crash.frames.len(), FRAMES, "the top {FRAMES} of 10");
    assert_eq!(crash.frames[0], "libswiftCore.dylib _assertionFailure(_:_:file:line:flags:)");
    assert_eq!(crash.frames[3], "Crashy closure #1 in AppDelegate.application(_:didFinishLaunchingWithOptions:)");
}

#[test]
fn an_uncaught_exception_reads_as_its_reason_and_where_it_was_thrown() {
    let crash = parse_ips(UNCAUGHT).unwrap().crash;
    assert_eq!(crash.exception, "EXC_CRASH (SIGABRT)");
    assert!(crash.reason.contains("NSInvalidArgumentException") && crash.reason.contains("unrecognized selector"));
    assert_eq!(crash.frames, ["CoreFoundation __exceptionPreprocess", "libobjc.A.dylib objc_exception_throw", "Shop +0x1e"]);
}

#[test]
fn a_report_that_is_not_one_is_skipped() {
    assert_eq!(parse_ips(""), None);
    assert_eq!(parse_ips("{}\nnot json"), None);
    assert_eq!(parse_ips("{\"bug_type\":\"211\"}\n{}"), None, "no incident id");
}

/// A home whose DiagnosticReports holds `files`.
fn home_with(files: &[(&str, &str)]) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("Library/Logs/DiagnosticReports");
    std::fs::create_dir_all(&dir).unwrap();
    for (name, body) in files {
        std::fs::write(dir.join(name), body).unwrap();
    }
    home
}

#[test]
fn a_simulators_reports_are_its_own_apps_once_each() {
    let other = SWIFT_TRAP.replace(UDID, "81CE1BE8-E38A-4BA8-8AAB-5DACA07576B3").replace("B8E1C770", "AAAAAAAA");
    let home = home_with(&[
        ("Crashy-1.ips", SWIFT_TRAP),
        ("Crashy-copy.ips", SWIFT_TRAP),
        ("Shop-1.ips", UNCAUGHT),
        ("Other.ips", &other),
        ("notes.diag", "x"),
    ]);
    let since = SystemTime::now() - Duration::from_secs(60);
    let all = sim_reports(home.path(), UDID, since, None);
    assert_eq!(all.len(), 2, "one per incident, this device's only: {all:?}");
    let shop = sim_reports(home.path(), UDID, since, Some("com.example.shop"));
    assert_eq!(shop.iter().map(|c| c.app.as_str()).collect::<Vec<_>>(), ["com.example.shop"]);
    let later = SystemTime::now() + Duration::from_secs(60);
    assert!(sim_reports(home.path(), UDID, later, None).is_empty(), "older than the window");
}

#[test]
fn the_report_watch_ignores_what_was_there_first() {
    let home = home_with(&[("Crashy-1.ips", SWIFT_TRAP)]);
    let mut scan = ReportScan::default();
    assert!(scan.poll(home.path(), UDID).is_empty(), "the first look is a baseline");
    assert!(scan.poll(home.path(), UDID).is_empty());
    let dir = home.path().join("Library/Logs/DiagnosticReports");
    // A report that lands now for a crash from before the first look.
    std::fs::write(dir.join("Late.ips"), SWIFT_TRAP.replace("B8E1C770", "CCCCCCCC")).unwrap();
    // Reports of crashes after it (the fixtures' times are in 2026).
    let now = now_capture();
    std::fs::write(dir.join("Half.ips"), "{\"incident_id\":").unwrap();
    std::fs::write(dir.join("Shop-1.ips"), UNCAUGHT.replace("2026-10-12 16:00:50.1000 +0700", &now)).unwrap();
    let new = scan.poll(home.path(), UDID);
    assert_eq!(new.iter().map(|c| c.app.as_str()).collect::<Vec<_>>(), ["com.example.shop"]);
    assert!(scan.poll(home.path(), UDID).is_empty(), "each report once");
    // The half-written one completes: it shows then.
    std::fs::write(dir.join("Half.ips"), SWIFT_TRAP.replace("B8E1C770", "BBBBBBBB").replace("2026-10-12 15:54:29.8027 +0700", &now)).unwrap();
    assert_eq!(scan.poll(home.path(), UDID).len(), 1);
}

/// This moment as an `.ips` capture time, a second ahead (in UTC).
fn now_capture() -> String {
    let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64 + 1;
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil from days (Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.0000 +0000", rest / 3600, rest % 3600 / 60, rest % 60)
}

#[test]
fn a_capture_time_reads_as_its_moment() {
    let at = capture_time("2026-10-12 15:54:29.8027 +0700").unwrap();
    let secs = at.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs_f64();
    // 2026-10-12 08:54:29.8027 UTC.
    assert!((secs - 1_791_795_269.802_7).abs() < 0.001, "{secs}");
    assert_eq!(capture_time("2026-10-12 08:54:29 +0000"), Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_791_795_269)));
    assert_eq!(capture_time("yesterday"), None);
}

#[test]
fn a_fatal_errors_words_become_its_reports_reason() {
    let mut crashes = vec![parse_ips(SWIFT_TRAP).unwrap().crash];
    give_reason(&mut crashes, "2026-10-12 15:54:29.797 Df Crashy[61726:59a99a] (libswiftCore.dylib) Swift/ContiguousArrayBuffer.swift:691: Fatal error: Index out of range");
    assert_eq!(crashes[0].reason, "Fatal error: Index out of range");
    give_reason(&mut crashes, "2026-10-12 15:54:29.797 Df Other[1:2] Fatal error: not this one");
    assert_eq!(crashes[0].reason, "Fatal error: Index out of range", "another process's words");
    let mut thrown = vec![parse_ips(UNCAUGHT).unwrap().crash];
    give_reason(&mut thrown, "2026-10-12 16:00:50.000 E  Shop[70001:1] Fatal error: later words");
    assert!(thrown[0].reason.contains("NSInvalidArgumentException"), "the report's own reason wins");
}

#[test]
fn a_fatal_line_without_a_report_yet_is_listed_as_such() {
    let mut crashes = Vec::new();
    unreported(&mut crashes, "2026-10-12 16:20:25.311 Df Crashy[61886:5d9f3b] Swift/A.swift:1: Fatal error: Index out of range");
    assert_eq!(crashes.len(), 1);
    assert_eq!((crashes[0].app.as_str(), crashes[0].time.as_str()), ("Crashy", "2026-10-12 16:20:25.311"));
    assert_eq!(crashes[0].reason, "Fatal error: Index out of range");
    assert!(crashes[0].exception.contains("still being written"));
    unreported(&mut crashes, "2026-10-12 16:20:25.400 Df Crashy[61886:5d9f3b] Fatal error: again");
    assert_eq!(crashes.len(), 1, "one per process");
}

#[test]
fn a_simulators_crashes_come_with_the_logs_warnings() {
    let home = home_with(&[("Crashy-1.ips", SWIFT_TRAP)]);
    let log = "Timestamp               Ty Process[PID:TID]\n\
               2026-10-12 15:54:29.797 Df Crashy[61726:59a99a] Swift/A.swift:1: Fatal error: Index out of range\n\
               2026-10-12 15:54:20.000 Df Crashy[61726:59a99a] Possible Unhandled Promise Rejection (id: 0)\n";
    let udid = SimUdid::new(&crate::DeviceId(UDID.into())).unwrap();
    let mut runner = ScriptedRunner::default();
    let args = {
        let any = markers::MARKERS.iter().map(|(m, _)| format!("eventMessage CONTAINS \"{m}\"")).collect::<Vec<_>>().join(" OR ");
        sim_args(&udid, 600, Level::Default, Some(&any)).join(" ")
    };
    runner = runner.expect(&format!("xcrun {args}"), CmdOutput::ok(log));
    let list = sim_crashes(&runner, home.path(), &udid, 600, None).unwrap();
    assert_eq!(list.crashes.len(), 1);
    assert_eq!(list.crashes[0].reason, "Fatal error: Index out of range");
    assert_eq!(list.warnings.len(), 1);
    assert!(list.warnings[0].contains("Unhandled Promise Rejection"));
}

#[test]
fn a_threadtime_line_splits_into_its_fields() {
    let line = parse_threadtime("10-12 16:05:01.120  4321  4321 E AndroidRuntime: FATAL EXCEPTION: main").unwrap();
    assert_eq!((line.time, line.pid, line.level, line.tag, line.msg), ("10-12 16:05:01.120", 4321, 'E', "AndroidRuntime", "FATAL EXCEPTION: main"));
    let padded = parse_threadtime("10-12 16:06:12.500  5555  5570 F libc    : Fatal signal 11").unwrap();
    assert_eq!(padded.tag, "libc");
    assert_eq!(parse_threadtime("--------- beginning of crash"), None);
}

#[test]
fn the_crash_buffer_reads_as_a_java_and_a_native_crash() {
    let crashes = parse_crash_buffer(LOGCAT);
    assert_eq!(crashes.len(), 2, "{crashes:?}");
    let java = &crashes[0];
    assert_eq!(java.app, "com.example.app");
    assert_eq!(java.exception, "java.lang.IllegalStateException");
    assert_eq!(java.reason, "Checkout failed: token=abc123secret");
    assert_eq!(java.frames.len(), 3, "the first exception's frames, not its cause's");
    assert_eq!(java.frames[0], "com.example.app.Checkout.pay(Checkout.kt:42)");
    assert_eq!(java.pid, Some(4321));
    let native = &crashes[1];
    assert_eq!((native.app.as_str(), native.exception.as_str(), native.pid), ("com.example.game", "SIGSEGV", Some(5555)));
    assert!(native.reason.starts_with("signal 11 (SIGSEGV), code 1"), "{}", native.reason);
    assert_eq!(native.frames[0], "/data/app/com.example.game/lib/arm64/libgame.so (Renderer::draw()+40)");
    assert_eq!(native.frames.len(), 3);
    assert_eq!(parse_crash_buffer(LOGCAT), crashes, "the same crash keeps its id");
}

#[test]
fn a_native_crash_keeps_its_id_while_its_backtrace_arrives() {
    let signal_only: String = LOGCAT.lines().take_while(|l| !l.contains("F DEBUG")).map(|l| format!("{l}\n")).collect();
    let early = parse_crash_buffer(&signal_only);
    let late = parse_crash_buffer(LOGCAT);
    assert_eq!(early[1].frames.len(), 0);
    assert_eq!(early[1].id, late[1].id, "the frames do not change the id");
}

#[test]
fn another_processs_backtrace_is_not_this_crashs() {
    let text = "10-12 16:06:12.500  5555  5570 F libc    : Fatal signal 6 (SIGABRT), code -1 in tid 5570 (main), pid 5555 (com.a.b)\n\
                10-12 16:06:12.700  5600  5600 F DEBUG   : pid: 7777, tid: 7777, name: other  >>> com.other <<<\n\
                10-12 16:06:12.701  5600  5600 F DEBUG   :       #00 pc 000000000004e1a8  /system/lib64/libother.so (x+1)\n\
                10-12 16:06:13.000  1200  1200 E AndroidRuntime: *** FATAL EXCEPTION IN SYSTEM PROCESS: main\n\
                10-12 16:06:13.000  1200  1200 E AndroidRuntime: java.lang.IllegalStateException: boom\n";
    let crashes = parse_crash_buffer(text);
    assert_eq!(crashes.len(), 2);
    assert!(crashes[0].frames.is_empty(), "{:?}", crashes[0].frames);
    assert_eq!(crashes[1].exception, "java.lang.IllegalStateException");
}

#[test]
fn an_apps_crash_includes_its_other_processes() {
    let crash = |app: &str| Crash { id: String::new(), time: String::new(), app: app.into(), exception: String::new(), reason: String::new(), frames: vec![], pid: None };
    assert!(is_app(&crash("com.x"), "com.x"));
    assert!(is_app(&crash("com.x:remote"), "com.x"));
    assert!(!is_app(&crash("com.xy"), "com.x"));
}

#[test]
fn the_buffer_watch_ignores_what_was_there_first() {
    let mut scan = BufferScan::default();
    let first = parse_crash_buffer(LOGCAT);
    assert!(scan.take(first.clone()).is_empty());
    assert!(scan.take(first).is_empty());
    let more = format!("{LOGCAT}10-12 16:09:00.000  7000  7000 E AndroidRuntime: FATAL EXCEPTION: main\n10-12 16:09:00.000  7000  7000 E AndroidRuntime: Process: com.example.app, PID: 7000\n10-12 16:09:00.000  7000  7000 E AndroidRuntime: java.lang.NullPointerException\n");
    let new = scan.take(parse_crash_buffer(&more));
    assert_eq!(new.len(), 1);
    assert_eq!(new[0].exception, "java.lang.NullPointerException");
}

#[test]
fn android_crashes_read_both_buffers_from_the_devices_clock() {
    let anrs = "10-12 16:07:00.000  900  950 E ActivityManager: ANR in com.example.app (com.example.app/.Main)\n\
                10-12 16:07:01.000  900  950 E ActivityManager: ANR in com.example.app.other (x)\n\
                10-12 16:07:09.000  900  950 E ActivityManager: ANR in com.example.app:remote (y)\n";
    let runner = ScriptedRunner::default()
        .expect("adb -s S shell date +%s", CmdOutput::ok("1760000600\n"))
        .expect("adb -s S logcat -d -v threadtime -T 1760000000.000 -b crash", CmdOutput::ok(LOGCAT))
        .expect("adb -s S logcat -d -v threadtime -T 1760000000.000 -b system", CmdOutput::ok(anrs))
        .expect("adb -s S shell pm list packages -U com.example.app", CmdOutput::ok("package:com.example.app uid:10123\n"))
        .expect(
            "adb -s S logcat -d -v threadtime -T 1760000000.000 -b main --uid=10123",
            CmdOutput::ok("10-12 16:07:03.000 4321 4400 W ReactNativeJS: Possible Unhandled Promise Rejection (id: 0):\n"),
        );
    let adb = Adb::new(&runner, Path::new("adb"));
    let list = android_crashes(&adb, "S", 600, Some("com.example.app")).unwrap();
    assert_eq!(list.crashes.len(), 1);
    assert_eq!(list.crashes[0].app, "com.example.app");
    // Its own ANR and its process's, never a look-alike package's; and what
    // it wrote itself.
    assert_eq!(list.warnings.len(), 3, "{:?}", list.warnings);
    assert!(!list.warnings.iter().any(|w| w.contains("com.example.app.other")));
    // Back in time order across the two reads: the newest last.
    assert!(list.warnings[1].contains("Unhandled Promise Rejection"), "{:?}", list.warnings);
    assert!(list.warnings[2].contains("com.example.app:remote"));
}

#[test]
fn a_reply_keeps_the_newest_crashes_and_says_so() {
    let crashes: Vec<Crash> = (0..25).map(|i| Crash { id: i.to_string(), ..parse_ips(SWIFT_TRAP).unwrap().crash }).collect();
    let (kept, left_out) = keep_newest(crashes);
    assert_eq!((kept.len(), left_out, kept[0].id.as_str()), (20, 5, "5"));
    assert!(newest(vec![], left_out)[0].contains("5 older crashes left out"));
}

#[test]
fn a_crash_named_late_is_announced_once_its_app_is_known() {
    let head = "10-12 16:09:00.000  7000  7000 E AndroidRuntime: FATAL EXCEPTION: main\n";
    let mut scan = BufferScan::default();
    assert!(scan.take(vec![]).is_empty(), "baseline");
    assert!(scan.take(parse_crash_buffer(head)).is_empty(), "no Process: line yet");
    let named = format!("{head}10-12 16:09:00.000  7000  7000 E AndroidRuntime: Process: com.a.b, PID: 7000\n10-12 16:09:00.000  7000  7000 E AndroidRuntime: java.lang.Error: x\n");
    assert_eq!(scan.take(parse_crash_buffer(&named)).len(), 1);
    // A frame arriving later updates the announced crash.
    let framed = format!("{named}10-12 16:09:00.000  7000  7000 E AndroidRuntime: \tat com.a.b.Main.go(Main.kt:1)\n");
    let update = scan.take(parse_crash_buffer(&framed));
    assert_eq!(update.len(), 1);
    assert_eq!(update[0].frames.len(), 1);
    assert!(scan.take(parse_crash_buffer(&framed)).is_empty());
}

#[test]
fn a_forgotten_crash_is_announced_again() {
    let mut scan = BufferScan::default();
    assert!(scan.take(vec![]).is_empty(), "baseline");
    let new = scan.take(parse_crash_buffer(LOGCAT));
    assert_eq!(new.len(), 2);
    scan.forget(&new);
    assert_eq!(scan.take(parse_crash_buffer(LOGCAT)).len(), 2, "looked at again, as new");
    assert!(scan.take(parse_crash_buffer(LOGCAT)).is_empty());
}

#[test]
fn a_baseline_crash_finishing_later_is_never_announced() {
    let head = "10-12 16:09:00.000  7000  7000 E AndroidRuntime: FATAL EXCEPTION: main\n";
    let mut scan = BufferScan::default();
    assert!(scan.take(parse_crash_buffer(head)).is_empty(), "baseline");
    let named = format!("{head}10-12 16:09:00.000  7000  7000 E AndroidRuntime: Process: com.a.b, PID: 7000\n10-12 16:09:00.000  7000  7000 E AndroidRuntime: \tat x.y(Z.kt:1)\n");
    assert!(scan.take(parse_crash_buffer(&named)).is_empty());
}

#[test]
fn two_java_crashes_interleaved_keep_their_own_lines() {
    let text = "10-12 16:10:00.000  1000  1000 E AndroidRuntime: FATAL EXCEPTION: main\n\
                10-12 16:10:00.000  2000  2000 E AndroidRuntime: FATAL EXCEPTION: main\n\
                10-12 16:10:00.001  1000  1000 E AndroidRuntime: Process: com.one, PID: 1000\n\
                10-12 16:10:00.001  2000  2000 E AndroidRuntime: Process: com.two, PID: 2000\n\
                10-12 16:10:00.002  1000  1000 E AndroidRuntime: java.lang.Error: one\n\
                10-12 16:10:00.002  2000  2000 E AndroidRuntime: java.lang.Error: two\n\
                10-12 16:10:00.003  1000  1000 E AndroidRuntime: \tat one.A(A.kt:1)\n";
    let crashes = parse_crash_buffer(text);
    assert_eq!(crashes.iter().map(|c| (c.app.as_str(), c.reason.as_str(), c.frames.len())).collect::<Vec<_>>(), [("com.one", "one", 1), ("com.two", "two", 0)]);
}
