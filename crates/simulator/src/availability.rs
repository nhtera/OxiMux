//! Whether the iOS Simulator panel can run at all on this Mac, checked once
//! up front so the panel can show one clear reason instead of a cascade of
//! `xcrun` failures.
//!
//! **Hard rule:** step 0 is `xcode-select -p`. If that fails, or names the
//! Command Line Tools rather than a full Xcode install, [`check`] never runs
//! `xcrun` or `xcodebuild` afterwards — a Mac with only the CLT would
//! otherwise hit Xcode's "install additional tools" dialog the moment this
//! crate touched `xcrun`, which is exactly what gates like this one exist to
//! avoid. `tests::no_xcrun_when_xcode_select_fails` and
//! `tests::no_xcrun_when_clt_only` pin it via [`ScriptedRunner`]'s call list.
//!
//! [`check`] itself is a handful of blocking subprocess calls (tens of
//! milliseconds on a warm Mac, longer the first time `xcodebuild` touches a
//! fresh Xcode install) — **never call it from `render`**; run it on a
//! background executor and hand the UI the [`Availability`] it produces.
//! [`CachedAvailability`] exists so a UI that asks "is it ready?" on every
//! frame doesn't re-run those subprocesses every time.

#[cfg(debug_assertions)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::runner::Runner;
use crate::simctl::{self, RuntimeInfo};

/// How long [`CachedAvailability`] reuses a result before recomputing.
pub const CACHE_TTL: Duration = Duration::from_secs(5);

/// What `xcode-select -p` (and, if it's allowed to run, `xcodebuild
/// -version`) found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Xcode {
    /// A full Xcode install is selected. `version` is `None` when
    /// `xcodebuild -version` itself failed (e.g. an unaccepted license) —
    /// that failure is reported through [`Support::Unsupported`], not by
    /// panicking here.
    Found { path: PathBuf, version: Option<String> },
    /// `xcode-select -p` failed: no developer directory is selected at all.
    Missing,
    /// `xcode-select -p` points anywhere but an `….app/Contents/Developer`
    /// (normally `/Library/Developer/CommandLineTools`): no full Xcode.app.
    CommandLineToolsOnly,
}

/// Whether this Xcode version is one the panel can run against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Support {
    /// Xcode 26.x: the version this panel was built and verified against.
    Supported,
    /// Xcode 27+: probably fine (the simulator wire protocol has been
    /// stable), but not yet verified.
    BestEffort,
    /// Xcode < 26, or the version could not be determined at all. The
    /// `String` is a person-facing reason, shown verbatim.
    Unsupported(String),
}

/// Where the `oximux-sim-helper` binary was found, if at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HelperStatus {
    Found(PathBuf),
    /// A person-facing reason (from [`oximux_sibling_binary::LocateError`]).
    Missing(String),
}

/// Everything [`check`] found, and enough to explain to a person why the
/// panel isn't ready yet ([`Availability::blocking_reason`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Availability {
    pub xcode: Xcode,
    pub support: Support,
    pub macos_ok: bool,
    pub arch_ok: bool,
    /// Installed, available iOS runtimes only — the ones a device can
    /// actually be created or booted against. Empty whenever `xcrun` was
    /// never allowed to run (see the module docs' hard rule).
    pub ios_runtimes: Vec<RuntimeInfo>,
    pub helper: HelperStatus,
}

impl Availability {
    /// `None` when the panel is fully usable; otherwise the single most
    /// actionable reason it isn't, in the order a person should fix things
    /// (Xcode itself, then support, then the OS/hardware floor, then
    /// runtimes, then the helper binary last since it ships with the app and
    /// is the least likely thing to be missing).
    pub fn blocking_reason(&self) -> Option<String> {
        match &self.xcode {
            Xcode::Missing => {
                return Some(
                    "Xcode is not installed. Install it from the App Store, then open it once.".into(),
                );
            }
            Xcode::CommandLineToolsOnly => {
                return Some(
                    "Only the Command Line Tools are installed. Open Xcode once so macOS selects \
                     it as the active developer directory."
                        .into(),
                );
            }
            Xcode::Found { .. } => {}
        }
        if let Support::Unsupported(reason) = &self.support {
            return Some(reason.clone());
        }
        if !self.macos_ok {
            return Some("macOS 14 or later is required for the iOS Simulator panel.".into());
        }
        if !self.arch_ok {
            return Some("the iOS Simulator panel requires Apple silicon (arm64).".into());
        }
        if self.ios_runtimes.is_empty() {
            return Some(
                "no iOS runtime is installed. In Xcode, go to Settings \u{2192} Platforms and \
                 install one."
                    .into(),
            );
        }
        match &self.helper {
            HelperStatus::Found(_) => None,
            HelperStatus::Missing(detail) => Some(format!("the simulator helper is missing: {detail}")),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.blocking_reason().is_none()
    }
}

/// How [`check`] finds the helper binary, injected so tests never touch the
/// filesystem. Any `Fn() -> HelperStatus` works, including a plain closure;
/// [`default_helper_probe`] is the production implementation.
pub trait HelperProbe {
    fn probe(&self) -> HelperStatus;
}

impl<F: Fn() -> HelperStatus> HelperProbe for F {
    fn probe(&self) -> HelperStatus {
        self()
    }
}

/// Runs every check and assembles an [`Availability`]. Blocking: several
/// subprocess spawns. See the module docs for the `xcrun`/`xcodebuild` gate
/// and the "never from `render`" rule.
pub fn check(runner: &dyn Runner, timeout: Duration, helper: &dyn HelperProbe) -> Availability {
    let xcode = probe_xcode(runner, timeout);
    let support = derive_support(&xcode);
    let ios_runtimes = match &xcode {
        Xcode::Found { .. } => simctl::list_runtimes(runner, timeout)
            .map(|runtimes| {
                runtimes.into_iter().filter(|r| r.platform == "iOS" && r.is_available).collect()
            })
            .unwrap_or_default(),
        Xcode::Missing | Xcode::CommandLineToolsOnly => Vec::new(),
    };
    Availability {
        xcode,
        support,
        macos_ok: macos_at_least_14(runner, timeout),
        arch_ok: std::env::consts::ARCH == "aarch64",
        ios_runtimes,
        helper: helper.probe(),
    }
}

fn probe_xcode(runner: &dyn Runner, timeout: Duration) -> Xcode {
    let Ok(out) = runner.run("xcode-select", &["-p"], None, timeout) else {
        return Xcode::Missing;
    };
    if !out.success() {
        return Xcode::Missing;
    }
    let path = out.stdout_str().trim().trim_end_matches('/').to_owned();
    if path.is_empty() {
        return Xcode::Missing;
    }
    // Only a developer dir inside an app bundle is a full Xcode. Anything
    // else — the Command Line Tools at their usual path, a trailing-slash or
    // symlinked spelling of it — has no simulator, and running `xcodebuild`
    // there is exactly what pops the "install developer tools" dialog.
    if !is_xcode_app_developer_dir(&path) {
        return Xcode::CommandLineToolsOnly;
    }
    let version = xcodebuild_version(runner, timeout);
    Xcode::Found { path: PathBuf::from(path), version }
}

/// `…/<Name>.app/Contents/Developer`: the only shape a full Xcode's developer
/// directory takes (`xcode-select -p` and `DEVELOPER_DIR` both point there).
fn is_xcode_app_developer_dir(path: &str) -> bool {
    path.strip_suffix("/Contents/Developer").is_some_and(|app| app.ends_with(".app"))
}

/// `xcodebuild -version`'s first line is `Xcode <version>`; the second line
/// (build number) is discarded.
fn xcodebuild_version(runner: &dyn Runner, timeout: Duration) -> Option<String> {
    let out = runner.run("xcodebuild", &["-version"], None, timeout).ok()?;
    if !out.success() {
        return None;
    }
    out.stdout_str().lines().next()?.strip_prefix("Xcode ").map(|v| v.trim().to_owned())
}

fn derive_support(xcode: &Xcode) -> Support {
    match xcode {
        Xcode::Missing => Support::Unsupported("Xcode is not installed.".into()),
        Xcode::CommandLineToolsOnly => {
            Support::Unsupported("only the Command Line Tools are installed.".into())
        }
        Xcode::Found { version, .. } => match version.as_deref().and_then(major_version) {
            Some(26) => Support::Supported,
            Some(major) if major > 26 => Support::BestEffort,
            Some(_) => Support::Unsupported("Xcode 26 or later is required.".into()),
            None => Support::Unsupported("could not determine the Xcode version.".into()),
        },
    }
}

/// `sw_vers -productVersion` (e.g. `15.7.3`) is not gated by the
/// `xcode-select` hard rule: it is a base-OS tool, present with or without
/// Xcode, so this may run even when [`Xcode::Missing`].
fn macos_at_least_14(runner: &dyn Runner, timeout: Duration) -> bool {
    let Ok(out) = runner.run("sw_vers", &["-productVersion"], None, timeout) else {
        return false;
    };
    if !out.success() {
        return false;
    }
    major_version(out.stdout_str().trim()).is_some_and(|major| major >= 14)
}

/// The leading integer of a dotted version string (`"26.3"` → `26`,
/// `"15.7.3"` → `15`).
fn major_version(version: &str) -> Option<u32> {
    version.split(['.', ' ']).next()?.parse().ok()
}

/// Production [`HelperProbe`]: the sibling binary, or (debug builds only) a
/// dev-workflow fallback at `<workspace>/target/bundle-tools/`, where
/// `scripts/fetch-sim-helper.sh` stages the fetched (not cargo-built) helper
/// during local development. Release and CI builds always take the sibling
/// path — the app bundle carries the helper as a true sibling of the binary.
pub fn default_helper_probe() -> HelperStatus {
    match oximux_sibling_binary::locate("oximux-sim-helper", Some("OXIMUX_SIM_HELPER")) {
        Ok(path) => HelperStatus::Found(path),
        Err(e) => {
            #[cfg(debug_assertions)]
            if let Some(path) = debug_bundle_tools_fallback() {
                return HelperStatus::Found(path);
            }
            HelperStatus::Missing(e.to_string())
        }
    }
}

#[cfg(debug_assertions)]
fn debug_bundle_tools_fallback() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let candidate = bundle_tools_path_from_exe(&exe)?;
    candidate.exists().then_some(candidate)
}

/// `target/<profile>/<exe>` → `target/bundle-tools/oximux-sim-helper`. A pure
/// function so the derivation is testable without controlling `current_exe`;
/// note it assumes the *app* binary's layout (`target/<profile>/<exe>`), not
/// a `cargo test` binary's (`target/<profile>/deps/<exe>-<hash>`), since only
/// the shipped app calls [`default_helper_probe`] in anger.
#[cfg(debug_assertions)]
fn bundle_tools_path_from_exe(exe: &Path) -> Option<PathBuf> {
    let target_dir = exe.parent()?.parent()?;
    Some(target_dir.join("bundle-tools").join(oximux_sibling_binary::sibling_file_name("oximux-sim-helper")))
}

/// Reuses an [`Availability`] for [`CACHE_TTL`] instead of re-running
/// `check`'s subprocesses on every call. `now` is a parameter rather than an
/// internal `Instant::now()` so tests can move time forward without a real
/// sleep; production callers just pass `Instant::now()`.
pub struct CachedAvailability {
    ttl: Duration,
    cached: Mutex<Option<(Instant, Availability)>>,
}

impl CachedAvailability {
    pub fn new(ttl: Duration) -> Self {
        Self { ttl, cached: Mutex::new(None) }
    }

    /// The cached value if `now` is within `ttl` of the last refresh, else
    /// the result of `compute` (cached at `now` for next time). `compute` is
    /// only invoked on a cache miss.
    pub fn get(&self, now: Instant, compute: impl FnOnce() -> Availability) -> Availability {
        let mut guard = self.cached.lock().unwrap();
        if let Some((at, avail)) = guard.as_ref()
            && now.saturating_duration_since(*at) < self.ttl
        {
            return avail.clone();
        }
        let avail = compute();
        *guard = Some((now, avail.clone()));
        avail
    }
}

impl Default for CachedAvailability {
    fn default() -> Self {
        Self::new(CACHE_TTL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{CmdOutput, ScriptedRunner};

    const T: Duration = Duration::from_secs(5);

    fn missing_helper() -> HelperStatus {
        HelperStatus::Missing("test: helper not probed".into())
    }

    fn found_helper() -> HelperStatus {
        HelperStatus::Found(PathBuf::from("/fake/oximux-sim-helper"))
    }

    #[test]
    fn no_xcrun_when_xcode_select_fails() {
        let runner = ScriptedRunner::default()
            .expect_spawn_error("xcode-select -p", "no such file")
            .expect("sw_vers -productVersion", CmdOutput::ok("15.7.3\n"));
        let avail = check(&runner, T, &missing_helper);
        assert_eq!(avail.xcode, Xcode::Missing);
        assert!(avail.ios_runtimes.is_empty());
        assert_eq!(runner.calls(), vec!["xcode-select -p", "sw_vers -productVersion"]);
    }

    #[test]
    fn no_xcrun_when_clt_only() {
        let runner = ScriptedRunner::default()
            .expect("xcode-select -p", CmdOutput::ok("/Library/Developer/CommandLineTools\n"))
            .expect("sw_vers -productVersion", CmdOutput::ok("15.7.3\n"));
        let avail = check(&runner, T, &missing_helper);
        assert_eq!(avail.xcode, Xcode::CommandLineToolsOnly);
        assert!(avail.ios_runtimes.is_empty());
        assert_eq!(runner.calls(), vec!["xcode-select -p", "sw_vers -productVersion"]);
        assert!(!avail.is_ready());
    }

    #[test]
    fn only_an_app_bundle_developer_dir_counts_as_xcode() {
        for (path, xcode) in [
            ("/Applications/Xcode.app/Contents/Developer", true),
            ("/Applications/Xcode-26.3.app/Contents/Developer/", true),
            ("/Library/Developer/CommandLineTools/", false),
            ("/opt/devtools", false),
            ("/Applications/Contents/Developer", false),
        ] {
            let trimmed = path.trim_end_matches('/');
            assert_eq!(is_xcode_app_developer_dir(trimmed), xcode, "{path}");
        }
        // A trailing slash on the CLT path still never reaches xcodebuild.
        let runner = ScriptedRunner::default()
            .expect("xcode-select -p", CmdOutput::ok("/Library/Developer/CommandLineTools/\n"))
            .expect("sw_vers -productVersion", CmdOutput::ok("15.7.3\n"));
        assert_eq!(check(&runner, T, &missing_helper).xcode, Xcode::CommandLineToolsOnly);
        assert_eq!(runner.calls(), vec!["xcode-select -p", "sw_vers -productVersion"]);
    }

    fn found_xcode_calls(runner: ScriptedRunner, xcode_version_stdout: &str) -> ScriptedRunner {
        runner
            .expect("xcode-select -p", CmdOutput::ok("/Applications/Xcode.app/Contents/Developer\n"))
            .expect("xcodebuild -version", CmdOutput::ok(xcode_version_stdout))
    }

    #[test]
    fn xcode_26_is_supported() {
        let runner = found_xcode_calls(ScriptedRunner::default(), "Xcode 26.3\nBuild version 17C529\n")
            .expect(
                "xcrun simctl list runtimes -j",
                CmdOutput::ok(r#"{"runtimes":[]}"#),
            )
            .expect("sw_vers -productVersion", CmdOutput::ok("15.7.3\n"));
        let avail = check(&runner, T, &missing_helper);
        assert_eq!(avail.support, Support::Supported);
    }

    #[test]
    fn xcode_27_is_best_effort() {
        let runner = found_xcode_calls(ScriptedRunner::default(), "Xcode 27.0\nBuild version 18A1\n")
            .expect("xcrun simctl list runtimes -j", CmdOutput::ok(r#"{"runtimes":[]}"#))
            .expect("sw_vers -productVersion", CmdOutput::ok("15.7.3\n"));
        let avail = check(&runner, T, &missing_helper);
        assert_eq!(avail.support, Support::BestEffort);
    }

    #[test]
    fn xcode_25_is_unsupported() {
        let runner = found_xcode_calls(ScriptedRunner::default(), "Xcode 25.4\nBuild version 16X1\n")
            .expect("xcrun simctl list runtimes -j", CmdOutput::ok(r#"{"runtimes":[]}"#))
            .expect("sw_vers -productVersion", CmdOutput::ok("15.7.3\n"));
        let avail = check(&runner, T, &missing_helper);
        assert!(matches!(avail.support, Support::Unsupported(_)));
        assert_eq!(
            avail.blocking_reason().as_deref(),
            Some("Xcode 26 or later is required.")
        );
    }

    #[test]
    fn macos_13_blocks_readiness() {
        let runner = found_xcode_calls(ScriptedRunner::default(), "Xcode 26.3\nBuild version 17C529\n")
            .expect("xcrun simctl list runtimes -j", CmdOutput::ok(r#"{"runtimes":[]}"#))
            .expect("sw_vers -productVersion", CmdOutput::ok("13.6\n"));
        let avail = check(&runner, T, &missing_helper);
        assert!(!avail.macos_ok);
        assert!(!avail.is_ready());
    }

    #[test]
    fn only_available_ios_runtimes_are_kept() {
        let runtimes_json = r#"{"runtimes":[
            {"identifier":"com.apple.CoreSimulator.SimRuntime.iOS-26-3","name":"iOS 26.3","version":"26.3.1","platform":"iOS","isAvailable":true},
            {"identifier":"com.apple.CoreSimulator.SimRuntime.iOS-17-0","name":"iOS 17.0","version":"17.0","platform":"iOS","isAvailable":false},
            {"identifier":"com.apple.CoreSimulator.SimRuntime.watchOS-11-0","name":"watchOS 11.0","version":"11.0","platform":"watchOS","isAvailable":true}
        ]}"#;
        let runner = found_xcode_calls(ScriptedRunner::default(), "Xcode 26.3\nBuild version 17C529\n")
            .expect("xcrun simctl list runtimes -j", CmdOutput::ok(runtimes_json))
            .expect("sw_vers -productVersion", CmdOutput::ok("15.7.3\n"));
        let avail = check(&runner, T, &found_helper);
        assert_eq!(avail.ios_runtimes.len(), 1);
        assert_eq!(avail.ios_runtimes[0].platform, "iOS");
        assert!(avail.is_ready(), "{:?}", avail.blocking_reason());
    }

    #[test]
    fn missing_helper_blocks_readiness_last() {
        let runtimes_json = r#"{"runtimes":[
            {"identifier":"com.apple.CoreSimulator.SimRuntime.iOS-26-3","name":"iOS 26.3","version":"26.3.1","platform":"iOS","isAvailable":true}
        ]}"#;
        let runner = found_xcode_calls(ScriptedRunner::default(), "Xcode 26.3\nBuild version 17C529\n")
            .expect("xcrun simctl list runtimes -j", CmdOutput::ok(runtimes_json))
            .expect("sw_vers -productVersion", CmdOutput::ok("15.7.3\n"));
        let avail = check(&runner, T, &missing_helper);
        assert!(!avail.is_ready());
        assert!(avail.blocking_reason().unwrap().contains("helper"));
    }

    #[test]
    #[cfg(debug_assertions)]
    fn bundle_tools_fallback_path_is_target_slash_bundle_tools() {
        let exe = PathBuf::from("/repo/target/debug/oximux");
        let got = bundle_tools_path_from_exe(&exe).unwrap();
        assert_eq!(got, PathBuf::from("/repo/target/bundle-tools/oximux-sim-helper"));
    }

    #[test]
    fn cached_availability_reuses_within_ttl_and_refreshes_after() {
        let cache = CachedAvailability::new(Duration::from_secs(5));
        let start = Instant::now();
        let calls = Mutex::new(0);
        let compute = || {
            *calls.lock().unwrap() += 1;
            Availability {
                xcode: Xcode::Missing,
                support: Support::Unsupported("x".into()),
                macos_ok: false,
                arch_ok: false,
                ios_runtimes: Vec::new(),
                helper: missing_helper(),
            }
        };

        cache.get(start, compute);
        cache.get(start + Duration::from_secs(4), compute);
        assert_eq!(*calls.lock().unwrap(), 1, "still within the 5s ttl");

        cache.get(start + Duration::from_secs(6), compute);
        assert_eq!(*calls.lock().unwrap(), 2, "ttl elapsed, must recompute");
    }
}
