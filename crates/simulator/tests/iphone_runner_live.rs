//! Live: the iPhone control runner end to end on a cabled phone — the build
//! (signed with a team on this Mac), a supervised launch, commands through
//! usbmux, and the shutdown. `#[ignore]`d like the other live tests:
//!
//! ```sh
//! OXIMUX_LIVE_TEAM=<team id> OXIMUX_RUNNER_TARBALL=<oximux-ios-runner-src-*.tar.gz> \
//!   cargo test -p oximux-simulator --test iphone_runner_live -- --ignored --nocapture
//! ```
//!
//! The build is kept under `target/iphone-runner-live/`, so a rerun only
//! launches. It presses Home on the phone once.
#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use oximux_simulator::child_ledger::Ledger;
use oximux_simulator::devicectl;
use oximux_simulator::ios_device::runner_build::{self, BuildRequest, RunnerHome};
use oximux_simulator::ios_device::runner_supervisor::{RunnerSpec, RunnerSupervisor, Transport};
use oximux_simulator::ios_device::usbmux::Usbmux;
use oximux_simulator::runner::{Runner, SystemRunner};
use serde_json::json;

const T: Duration = Duration::from_secs(30);

#[test]
#[ignore = "needs a cabled iPhone and a signing team; run with --ignored"]
fn the_runner_builds_launches_and_answers_over_usbmux() {
    let team = std::env::var("OXIMUX_LIVE_TEAM").expect("OXIMUX_LIVE_TEAM");
    let tarball = PathBuf::from(std::env::var("OXIMUX_RUNNER_TARBALL").expect("OXIMUX_RUNNER_TARBALL"));
    let runner = SystemRunner;
    let phone = devicectl::list(&runner, T).expect("devicectl");
    let id = devicectl::connected(&phone).next().expect("a cabled, paired iPhone").clone();
    let udid = devicectl::hardware_udid(&id).expect("an iosdev id").to_owned();
    eprintln!("phone {udid}");

    let xcodebuild = PathBuf::from(runner.run("xcrun", &["--find", "xcodebuild"], None, T).unwrap().stdout_str().trim());
    let version = runner.run(&xcodebuild.display().to_string(), &["-version"], None, T).unwrap().stdout_str();
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/iphone-runner-live");
    let home = RunnerHome::new(&target);
    let ledger = Arc::new(Ledger::open(target.join("children.json")).unwrap());
    let cancel = AtomicBool::new(false);
    let request = BuildRequest {
        home: &home,
        tarball: &tarball,
        team: &team,
        udid: &udid,
        xcodebuild: &xcodebuild,
        xcode_version: &version,
        ledger: Some(&ledger),
        cancel: &cancel,
    };
    let started = Instant::now();
    let mut lines = 0;
    let built = runner_build::ensure(&runner, &request, &mut |_| lines += 1).expect("the runner builds");
    eprintln!("build: {:.0?} ({lines} lines), expires {:?}", started.elapsed(), built.expires);
    assert!(runner_build::current(&runner, &request).is_some(), "a finished build is current");

    let supervisor = RunnerSupervisor::new(RunnerSpec {
        udid: udid.clone(),
        xcodebuild,
        xctestrun: built.xctestrun,
        derived: home.derived(&udid),
        ledger: Some(ledger.clone()),
        transport: Transport::Usbmux(Usbmux::system()),
    });
    let started = Instant::now();
    supervisor.start().expect("the runner listens");
    eprintln!("launch: {:.0?}", started.elapsed());
    assert_eq!(ledger.entries().unwrap().len(), 1, "the run is in the ledger");

    let status = supervisor.call("status", json!({})).unwrap();
    assert_eq!(status.data["protocol"], "oximux-runner/1");
    let viewport = supervisor.call("viewport", json!({})).unwrap();
    eprintln!("viewport {}", viewport.data);
    assert!(viewport.data["width"].as_f64().unwrap() > 300.0);
    let started = Instant::now();
    let snapshot = supervisor.call("snapshot", json!({})).unwrap();
    eprintln!("snapshot: {} nodes in {:.0?}", snapshot.data["nodes"].as_array().unwrap().len(), started.elapsed());
    let started = Instant::now();
    supervisor.call("button", json!({"name": "home"})).unwrap();
    eprintln!("home: {:.0?}", started.elapsed());

    supervisor.stop();
    assert!(!supervisor.is_running());
    assert!(ledger.entries().unwrap().is_empty(), "nothing left in the ledger");
}
