//! Whether the Mobile Emulator panel can run at all on this Mac, checked once
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
//! When the CLT are selected, [`check`] lists `Xcode*.app` bundles on disk
//! (a directory listing, no Xcode tool) so the panel can offer to select one.
//! It does the same when the selected Xcode is best-effort, to name an
//! installed Xcode 26 to switch to (its version read from the bundle's
//! `version.plist`, still no Xcode tool).
//!
//! [`check`] itself is a handful of blocking subprocess calls (tens of
//! milliseconds on a warm Mac, longer the first time `xcodebuild` touches a
//! fresh Xcode install) — **never call it from `render`**; run it on a
//! background executor and hand the UI the [`Availability`] it produces.
//! [`CachedAvailability`] exists so a UI that asks "is it ready?" on every
//! frame doesn't re-run those subprocesses every time.

use std::path::{Path, PathBuf};
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
    /// `installed` is an `Xcode*.app` found on disk anyway, as for
    /// [`Xcode::CommandLineToolsOnly`].
    Missing { installed: Option<PathBuf> },
    /// `xcode-select -p` points anywhere but an `….app/Contents/Developer`
    /// (normally `/Library/Developer/CommandLineTools`). `installed` is an
    /// `Xcode*.app` found on disk anyway: installing or opening Xcode never
    /// changes `xcode-select`, so a Mac that had the CLT first stays here
    /// until the developer directory is switched (see [`crate::xcode_app`]).
    CommandLineToolsOnly { installed: Option<PathBuf> },
}

impl Xcode {
    /// `Some(self)` when `xcode-select` names a full Xcode — what `xcrun` and
    /// the helper actually run against, path *and* version (an in-place
    /// upgrade swaps the frameworks under the same path). `None` for every
    /// unselected state, whatever Xcode may sit on disk: finding one there
    /// changes nothing the helpers loaded.
    pub fn selected(&self) -> Option<&Self> {
        matches!(self, Xcode::Found { .. }).then_some(self)
    }

    /// An `Xcode*.app` on disk that `xcode-select` does not name.
    pub fn unselected_app(&self) -> Option<&Path> {
        match self {
            Xcode::Missing { installed } | Xcode::CommandLineToolsOnly { installed } => installed.as_deref(),
            Xcode::Found { .. } => None,
        }
    }
}

/// The version the panel was built and verified against.
pub const VERIFIED_XCODE_MAJOR: u32 = 26;

/// Whether this Xcode version is one the panel can run against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Support {
    /// Xcode 26.x: the version this panel was built and verified against.
    Supported,
    /// Any other known version, older or newer: not verified, but not
    /// blocked either. The helper dlopens SimulatorKit from both its old and
    /// its Xcode 27 location, and when it cannot load it, it says so
    /// (`framework_load_failed`) and the panel offers to switch to Xcode 26.
    BestEffort,
    /// The version could not be determined, or no full Xcode is selected.
    /// The `String` is a person-facing reason, shown verbatim.
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
    /// An installed Xcode 26 that is not the selected one, looked for only
    /// when the selected Xcode is [`Support::BestEffort`]: the one to offer
    /// when the helper cannot load its frameworks.
    pub verified_xcode: Option<PathBuf>,
}

impl Availability {
    /// `None` when the panel is fully usable; otherwise the single most
    /// actionable reason it isn't, in the order a person should fix things
    /// (Xcode itself, then support, then the OS/hardware floor, then
    /// runtimes, then the helper binary last since it ships with the app and
    /// is the least likely thing to be missing).
    pub fn blocking_reason(&self) -> Option<String> {
        match &self.xcode {
            Xcode::Missing { installed: Some(app) } => {
                return Some(format!(
                    "{} is installed, but no developer directory is selected. Select it with: {}",
                    app.display(),
                    crate::xcode_app::select_command(app)
                ));
            }
            Xcode::Missing { installed: None } => {
                return Some(
                    "Xcode is not installed. Install it from the App Store, then open it once.".into(),
                );
            }
            Xcode::CommandLineToolsOnly { installed: Some(app) } => {
                return Some(format!(
                    "{} is installed, but the Command Line Tools are the active developer directory. \
                     Select it with: {}",
                    app.display(),
                    crate::xcode_app::select_command(app)
                ));
            }
            Xcode::CommandLineToolsOnly { installed: None } => {
                return Some("Only the Command Line Tools are installed. Install Xcode from the App Store.".into());
            }
            Xcode::Found { .. } => {}
        }
        if let Support::Unsupported(reason) = &self.support {
            return Some(reason.clone());
        }
        if !self.macos_ok {
            return Some("macOS 14 or later is required for the Mobile Emulator panel.".into());
        }
        if !self.arch_ok {
            return Some("the Mobile Emulator panel requires Apple silicon (arm64).".into());
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

    /// Whether the setup checklist's Xcode row passes: a full Xcode is
    /// selected *and* its version is not refused. Never a pass above a
    /// refusal (issue #41).
    pub fn xcode_ok(&self) -> bool {
        matches!(self.xcode, Xcode::Found { .. }) && !matches!(self.support, Support::Unsupported(_))
    }

    /// The warning to show when the selected Xcode is not the verified
    /// version; `None` for Xcode 26 and whenever no version is known. It
    /// does not check readiness: callers show it beside a usable panel.
    pub fn best_effort_note(&self) -> Option<String> {
        let Xcode::Found { version: Some(version), .. } = &self.xcode else { return None };
        (self.support == Support::BestEffort).then(|| {
            format!("Xcode {version} is supported on a best-effort basis. OxiMux is verified with Xcode {VERIFIED_XCODE_MAJOR}.")
        })
    }

    /// How to get onto Xcode 26 when a best-effort Xcode could not run the
    /// helper: the `xcode-select -s` command for an installed Xcode 26, or,
    /// when none turned up (only the Applications folders are searched, so
    /// that is not proof there is none), the general instruction.
    pub fn switch_to_verified_hint(&self) -> String {
        match &self.verified_xcode {
            Some(app) => format!(
                "If the simulator doesn't stream, select Xcode {VERIFIED_XCODE_MAJOR}: {}",
                crate::xcode_app::select_command(app)
            ),
            None => format!(
                "If the simulator doesn't stream, select Xcode {VERIFIED_XCODE_MAJOR} with \
                 sudo xcode-select -s '/path/to/Xcode.app/Contents/Developer', installing it first if needed."
            ),
        }
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
/// and the "never from `render`" rule. `xcode_apps` lists the `Xcode*.app`
/// bundles on disk ([`crate::xcode_app::installed_xcode_apps`] in
/// production); it is consulted only when `xcode-select` names none, or
/// names a best-effort one.
pub fn check(
    runner: &dyn Runner,
    timeout: Duration,
    helper: &dyn HelperProbe,
    xcode_apps: &dyn Fn() -> Vec<PathBuf>,
) -> Availability {
    let xcode = probe_xcode(runner, timeout, xcode_apps);
    let support = derive_support(&xcode);
    let verified_xcode = match &xcode {
        Xcode::Found { path, .. } if support == Support::BestEffort => verified_xcode_app(xcode_apps(), path),
        _ => None,
    };
    let ios_runtimes = match &xcode {
        Xcode::Found { .. } => simctl::list_runtimes(runner, timeout)
            .map(|runtimes| {
                runtimes.into_iter().filter(|r| r.platform == "iOS" && r.is_available).collect()
            })
            .unwrap_or_default(),
        Xcode::Missing { .. } | Xcode::CommandLineToolsOnly { .. } => Vec::new(),
    };
    Availability {
        xcode,
        support,
        macos_ok: macos_at_least_14(runner, timeout),
        arch_ok: std::env::consts::ARCH == "aarch64",
        ios_runtimes,
        helper: helper.probe(),
        verified_xcode,
    }
}

/// An Xcode 26 among `apps`, other than the one whose developer dir is
/// `selected`. Versions come from each bundle's `version.plist`, never from
/// its name (the App Store's bundle is just `Xcode.app`).
fn verified_xcode_app(apps: Vec<PathBuf>, selected: &Path) -> Option<PathBuf> {
    let candidates = apps
        .into_iter()
        .filter(|app| crate::xcode_app::developer_dir(app) != selected)
        .filter(|app| {
            crate::xcode_app::bundle_version(app).as_deref().and_then(major_version) == Some(VERIFIED_XCODE_MAJOR)
        })
        .collect();
    crate::xcode_app::pick(candidates)
}

fn probe_xcode(runner: &dyn Runner, timeout: Duration, xcode_apps: &dyn Fn() -> Vec<PathBuf>) -> Xcode {
    let missing = || Xcode::Missing { installed: crate::xcode_app::pick(xcode_apps()) };
    let Ok(out) = runner.run("xcode-select", &["-p"], None, timeout) else {
        return missing();
    };
    if !out.success() {
        return missing();
    }
    let path = out.stdout_str().trim().trim_end_matches('/').to_owned();
    if path.is_empty() {
        return missing();
    }
    // Only a developer dir inside an app bundle is a full Xcode. Anything
    // else — the Command Line Tools at their usual path, a trailing-slash or
    // symlinked spelling of it — has no simulator, and running `xcodebuild`
    // there is exactly what pops the "install developer tools" dialog.
    if !is_xcode_app_developer_dir(&path) {
        return Xcode::CommandLineToolsOnly { installed: crate::xcode_app::pick(xcode_apps()) };
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
        Xcode::Missing { .. } => Support::Unsupported("no developer directory is selected.".into()),
        Xcode::CommandLineToolsOnly { .. } => {
            Support::Unsupported("the Command Line Tools are the active developer directory.".into())
        }
        Xcode::Found { version, .. } => match version.as_deref().and_then(major_version) {
            Some(VERIFIED_XCODE_MAJOR) => Support::Supported,
            Some(_) => Support::BestEffort,
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

/// The capture helper's app bundle, as it ships inside `OxiMux.app`
/// (`Contents/Helpers/`, beside `Contents/MacOS/`).
pub const CAPTURE_APP: &str = "OxiMux Device Capture.app";
/// Its executable, inside [`CAPTURE_APP`].
const CAPTURE_EXE: &str = "Contents/MacOS/oximux-device-capture";
/// Points at a capture helper executable **inside its app bundle** (macOS
/// grants the camera to a bundle, not to a bare binary), for local builds.
pub const CAPTURE_OVERRIDE: &str = "OXIMUX_DEVICE_CAPTURE";

/// Where the capture helper is: [`CAPTURE_OVERRIDE`], else the bundled app,
/// else (debug builds) `target/bundle-tools/`, where a fetched release is
/// staged for local runs — the same policy as the simulator helper's.
pub fn default_capture_probe() -> HelperStatus {
    if let Some(path) = std::env::var_os(CAPTURE_OVERRIDE).filter(|v| !v.is_empty()).map(PathBuf::from) {
        return if path.is_file() {
            HelperStatus::Found(path)
        } else {
            HelperStatus::Missing(format!("{CAPTURE_OVERRIDE} points at {}, which does not exist", path.display()))
        };
    }
    let exe = std::env::current_exe().ok();
    let bundled = exe.as_deref().and_then(bundled_capture_path);
    if let Some(path) = bundled.as_ref().filter(|p| p.is_file()) {
        return HelperStatus::Found(path.clone());
    }
    #[cfg(debug_assertions)]
    if let Some(path) = exe.as_deref().and_then(dev_capture_path).filter(|p| p.is_file()) {
        return HelperStatus::Found(path);
    }
    HelperStatus::Missing(format!("{CAPTURE_APP} is not installed beside OxiMux (set {CAPTURE_OVERRIDE} for a local build)"))
}

/// `…/OxiMux.app/Contents/MacOS/oximux` → `…/Contents/Helpers/<app>/<exe>`.
fn bundled_capture_path(exe: &Path) -> Option<PathBuf> {
    let contents = exe.parent()?.parent()?;
    Some(contents.join("Helpers").join(CAPTURE_APP).join(CAPTURE_EXE))
}

/// `target/<profile>/<exe>` → `target/bundle-tools/<app>/<exe>`.
#[cfg(debug_assertions)]
fn dev_capture_path(exe: &Path) -> Option<PathBuf> {
    Some(exe.parent()?.parent()?.join("bundle-tools").join(CAPTURE_APP).join(CAPTURE_EXE))
}

/// Overrides where the iPhone runner's source tarball is (a local pack).
pub const RUNNER_OVERRIDE: &str = "OXIMUX_IOS_RUNNER";

/// The iPhone control runner's sources (`name`: the pinned tarball's file
/// name): [`RUNNER_OVERRIDE`], else the app's `Resources/`, else (debug
/// builds) `target/bundle-tools/`, where the fetch script stages it.
pub fn default_runner_tarball(name: &str) -> HelperStatus {
    if let Some(path) = std::env::var_os(RUNNER_OVERRIDE).filter(|v| !v.is_empty()).map(PathBuf::from) {
        return if path.is_file() {
            HelperStatus::Found(path)
        } else {
            HelperStatus::Missing(format!("{RUNNER_OVERRIDE} points at {}, which does not exist", path.display()))
        };
    }
    let exe = std::env::current_exe().ok();
    if let Some(path) = exe.as_deref().and_then(|e| Some(e.parent()?.parent()?.join("Resources").join(name))).filter(|p| p.is_file()) {
        return HelperStatus::Found(path);
    }
    #[cfg(debug_assertions)]
    if let Some(path) = exe.as_deref().and_then(|e| Some(e.parent()?.parent()?.join("bundle-tools").join(name))).filter(|p| p.is_file()) {
        return HelperStatus::Found(path);
    }
    HelperStatus::Missing(format!("this build of OxiMux has no iPhone runner sources ({name}; set {RUNNER_OVERRIDE} for a local pack)"))
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

    fn no_apps() -> Vec<PathBuf> {
        Vec::new()
    }

    fn missing_helper() -> HelperStatus {
        HelperStatus::Missing("test: helper not probed".into())
    }

    #[cfg(target_arch = "aarch64")] // its only test needs Apple silicon
    fn found_helper() -> HelperStatus {
        HelperStatus::Found(PathBuf::from("/fake/oximux-sim-helper"))
    }

    #[test]
    fn no_xcrun_when_xcode_select_fails() {
        let runner = ScriptedRunner::default()
            .expect_spawn_error("xcode-select -p", "no such file")
            .expect("sw_vers -productVersion", CmdOutput::ok("15.7.3\n"));
        let avail = check(&runner, T, &missing_helper, &no_apps);
        assert_eq!(avail.xcode, Xcode::Missing { installed: None });
        assert!(avail.ios_runtimes.is_empty());
        assert_eq!(runner.calls(), vec!["xcode-select -p", "sw_vers -productVersion"]);
    }

    #[test]
    fn no_xcrun_when_clt_only() {
        let runner = ScriptedRunner::default()
            .expect("xcode-select -p", CmdOutput::ok("/Library/Developer/CommandLineTools\n"))
            .expect("sw_vers -productVersion", CmdOutput::ok("15.7.3\n"));
        let avail = check(&runner, T, &missing_helper, &no_apps);
        assert_eq!(avail.xcode, Xcode::CommandLineToolsOnly { installed: None });
        assert!(avail.ios_runtimes.is_empty());
        assert_eq!(runner.calls(), vec!["xcode-select -p", "sw_vers -productVersion"]);
        assert!(!avail.is_ready());
    }

    /// Xcode installed after the CLT: `xcode-select` still names the CLT
    /// (installing or opening Xcode never switches it). The panel must name
    /// the Xcode it found and how to select it — and still never run `xcrun`.
    #[test]
    #[cfg(unix)] // asserts the `/`-joined developer dir in the message
    fn clt_selected_with_xcode_on_disk_names_the_app_and_the_switch() {
        let runner = ScriptedRunner::default()
            .expect("xcode-select -p", CmdOutput::ok("/Library/Developer/CommandLineTools\n"))
            .expect("sw_vers -productVersion", CmdOutput::ok("15.7.3\n"));
        let apps = || vec![PathBuf::from("/Applications/Xcode-beta.app"), PathBuf::from("/Applications/Xcode.app")];
        let avail = check(&runner, T, &missing_helper, &apps);
        assert_eq!(avail.xcode, Xcode::CommandLineToolsOnly { installed: Some(PathBuf::from("/Applications/Xcode.app")) });
        assert_eq!(runner.calls(), vec!["xcode-select -p", "sw_vers -productVersion"]);
        let reason = avail.blocking_reason().unwrap();
        assert!(reason.contains("/Applications/Xcode.app is installed"), "{reason}");
        assert!(reason.contains("sudo xcode-select -s '/Applications/Xcode.app/Contents/Developer'"), "{reason}");
        assert!(!reason.contains("Open Xcode once"), "opening Xcode never switches xcode-select: {reason}");
    }

    #[test]
    fn only_a_selected_xcode_counts_for_helper_restarts() {
        let found = |version: &str| Xcode::Found {
            path: PathBuf::from("/Applications/Xcode.app/Contents/Developer"),
            version: Some(version.into()),
        };
        // An in-place upgrade (same path, new version) is a change.
        assert_ne!(found("26.3").selected(), found("26.4").selected());
        // An unselected Xcode on disk is not: every unselected state is equal.
        let unselected = Xcode::CommandLineToolsOnly { installed: Some(PathBuf::from("/Applications/Xcode.app")) };
        assert_eq!(unselected.selected(), None);
        assert_eq!(Xcode::Missing { installed: None }.selected(), None);
        assert_eq!(unselected.unselected_app(), Some(Path::new("/Applications/Xcode.app")));
    }

    /// `xcode-select -p` failing outright (no developer dir, or a deleted
    /// one) still finds the Xcode on disk to offer — and never runs `xcrun`.
    #[test]
    #[cfg(unix)] // asserts the `/`-joined developer dir in the message
    fn no_developer_dir_with_xcode_on_disk_offers_it() {
        let runner = ScriptedRunner::default()
            .expect("xcode-select -p", CmdOutput::failed(2, "xcode-select: error: unable to get active developer directory"))
            .expect("sw_vers -productVersion", CmdOutput::ok("15.7.3\n"));
        let apps = || vec![PathBuf::from("/Applications/Xcode.app")];
        let avail = check(&runner, T, &missing_helper, &apps);
        assert_eq!(avail.xcode, Xcode::Missing { installed: Some(PathBuf::from("/Applications/Xcode.app")) });
        assert_eq!(runner.calls(), vec!["xcode-select -p", "sw_vers -productVersion"]);
        let reason = avail.blocking_reason().unwrap();
        assert!(reason.contains("sudo xcode-select -s '/Applications/Xcode.app/Contents/Developer'"), "{reason}");
    }

    #[test]
    fn a_selected_verified_xcode_never_scans_the_disk() {
        let runner = found_xcode_calls(ScriptedRunner::default(), "Xcode 26.3\nBuild version 17C529\n")
            .expect("xcrun simctl list runtimes -j", CmdOutput::ok(r#"{"runtimes":[]}"#))
            .expect("sw_vers -productVersion", CmdOutput::ok("15.7.3\n"));
        let apps = || -> Vec<PathBuf> { panic!("scanned for Xcode.app although xcode-select named one") };
        assert!(matches!(check(&runner, T, &missing_helper, &apps).xcode, Xcode::Found { .. }));
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
        assert_eq!(check(&runner, T, &missing_helper, &no_apps).xcode, Xcode::CommandLineToolsOnly { installed: None });
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
        let avail = check(&runner, T, &missing_helper, &no_apps);
        assert_eq!(avail.support, Support::Supported);
    }

    #[test]
    fn xcode_27_is_best_effort() {
        let runner = found_xcode_calls(ScriptedRunner::default(), "Xcode 27.0\nBuild version 18A1\n")
            .expect("xcrun simctl list runtimes -j", CmdOutput::ok(r#"{"runtimes":[]}"#))
            .expect("sw_vers -productVersion", CmdOutput::ok("15.7.3\n"));
        let avail = check(&runner, T, &missing_helper, &no_apps);
        assert_eq!(avail.support, Support::BestEffort);
    }

    /// Issue #41: Xcode 16.4, installed by Xcodes.app as `Xcode-16.4.0.app`,
    /// passed every checklist row yet was refused with "Xcode 26 or later is
    /// required". An older Xcode is best-effort like a newer one: usable,
    /// with a note, never a silent block.
    #[test]
    #[cfg(target_arch = "aarch64")] // readiness needs Apple silicon
    fn an_older_xcode_is_best_effort_not_a_block() {
        let runtimes_json = r#"{"runtimes":[
            {"identifier":"com.apple.CoreSimulator.SimRuntime.iOS-18-6","name":"iOS 18.6","version":"18.6","platform":"iOS","isAvailable":true}
        ]}"#;
        let runner = ScriptedRunner::default()
            .expect("xcode-select -p", CmdOutput::ok("/Applications/Xcode-16.4.0.app/Contents/Developer\n"))
            .expect("xcodebuild -version", CmdOutput::ok("Xcode 16.4\nBuild version 16F6\n"))
            .expect("xcrun simctl list runtimes -j", CmdOutput::ok(runtimes_json))
            .expect("sw_vers -productVersion", CmdOutput::ok("15.7.9\n"));
        let apps = || vec![PathBuf::from("/Applications/Xcode-16.4.0.app")];
        let avail = check(&runner, T, &found_helper, &apps);
        assert_eq!(avail.support, Support::BestEffort);
        assert!(avail.is_ready(), "{:?}", avail.blocking_reason());
        assert!(avail.xcode_ok());
        let note = avail.best_effort_note().unwrap();
        assert!(note.contains("Xcode 16.4") && note.contains("best-effort"), "{note}");
        // The only Xcode on disk is the selected one: nothing to switch to.
        assert_eq!(avail.verified_xcode, None);
        let hint = avail.switch_to_verified_hint();
        assert!(hint.contains("select Xcode 26") && hint.contains("installing it first if needed"), "{hint}");
    }

    #[test]
    fn an_unknown_xcode_version_still_blocks() {
        let runner = found_xcode_calls(ScriptedRunner::default(), "")
            .expect("xcrun simctl list runtimes -j", CmdOutput::ok(r#"{"runtimes":[]}"#))
            .expect("sw_vers -productVersion", CmdOutput::ok("15.7.3\n"));
        let avail = check(&runner, T, &missing_helper, &no_apps);
        assert!(matches!(avail.support, Support::Unsupported(_)));
        assert_eq!(avail.blocking_reason().as_deref(), Some("could not determine the Xcode version."));
        assert!(!avail.xcode_ok(), "the row must not pass above the refusal");
        assert_eq!(avail.best_effort_note(), None);
    }

    /// The switch hint names an Xcode 26 actually on disk, by its
    /// `version.plist` rather than its bundle name, and never the selected one.
    #[test]
    #[cfg(unix)] // asserts the `/`-joined developer dir in the command
    fn a_best_effort_xcode_finds_an_installed_xcode_26_to_switch_to() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = |name: &str, version: &str| {
            let app = dir.path().join(name);
            std::fs::create_dir_all(app.join("Contents")).unwrap();
            let plist = format!("<dict><key>CFBundleShortVersionString</key><string>{version}</string></dict>");
            std::fs::write(app.join("Contents").join("version.plist"), plist).unwrap();
            app
        };
        let old = bundle("Xcode-16.4.0.app", "16.4");
        let wired = |apps: Vec<PathBuf>| {
            let runner = ScriptedRunner::default()
                .expect("xcode-select -p", CmdOutput::ok(format!("{}\n", crate::xcode_app::developer_dir(&old).display())))
                .expect("xcodebuild -version", CmdOutput::ok("Xcode 16.4\nBuild version 16F6\n"))
                .expect("xcrun simctl list runtimes -j", CmdOutput::ok(r#"{"runtimes":[]}"#))
                .expect("sw_vers -productVersion", CmdOutput::ok("15.7.9\n"));
            check(&runner, T, &missing_helper, &move || apps.clone()).verified_xcode
        };
        let verified = bundle("Xcode.app", "26.3");
        let misnamed = bundle("Xcode-26.9.app", "27.0");
        let selected = crate::xcode_app::developer_dir(&old);

        let apps = vec![old.clone(), verified.clone(), misnamed.clone()];
        assert_eq!(verified_xcode_app(apps, &selected), Some(verified.clone()));
        assert_eq!(verified_xcode_app(vec![old.clone(), misnamed.clone()], &selected), None);
        // `check` wires it up for the selected best-effort Xcode.
        assert_eq!(wired(vec![old.clone(), verified.clone(), misnamed.clone()]), Some(verified.clone()));
        assert_eq!(wired(vec![old.clone(), misnamed]), None);

        let avail = Availability {
            xcode: Xcode::Found { path: selected, version: Some("16.4".into()) },
            support: Support::BestEffort,
            macos_ok: true,
            arch_ok: true,
            ios_runtimes: Vec::new(),
            helper: missing_helper(),
            verified_xcode: Some(verified.clone()),
        };
        let hint = avail.switch_to_verified_hint();
        assert!(hint.ends_with(&crate::xcode_app::select_command(&verified)), "{hint}");
    }

    #[test]
    fn macos_13_blocks_readiness() {
        let runner = found_xcode_calls(ScriptedRunner::default(), "Xcode 26.3\nBuild version 17C529\n")
            .expect("xcrun simctl list runtimes -j", CmdOutput::ok(r#"{"runtimes":[]}"#))
            .expect("sw_vers -productVersion", CmdOutput::ok("13.6\n"));
        let avail = check(&runner, T, &missing_helper, &no_apps);
        assert!(!avail.macos_ok);
        assert!(!avail.is_ready());
    }

    // Readiness also needs Apple silicon (`arch_ok` is this build's target).
    #[test]
    #[cfg(target_arch = "aarch64")]
    fn only_available_ios_runtimes_are_kept() {
        let runtimes_json = r#"{"runtimes":[
            {"identifier":"com.apple.CoreSimulator.SimRuntime.iOS-26-3","name":"iOS 26.3","version":"26.3.1","platform":"iOS","isAvailable":true},
            {"identifier":"com.apple.CoreSimulator.SimRuntime.iOS-17-0","name":"iOS 17.0","version":"17.0","platform":"iOS","isAvailable":false},
            {"identifier":"com.apple.CoreSimulator.SimRuntime.watchOS-11-0","name":"watchOS 11.0","version":"11.0","platform":"watchOS","isAvailable":true}
        ]}"#;
        let runner = found_xcode_calls(ScriptedRunner::default(), "Xcode 26.3\nBuild version 17C529\n")
            .expect("xcrun simctl list runtimes -j", CmdOutput::ok(runtimes_json))
            .expect("sw_vers -productVersion", CmdOutput::ok("15.7.3\n"));
        let avail = check(&runner, T, &found_helper, &no_apps);
        assert_eq!(avail.ios_runtimes.len(), 1);
        assert_eq!(avail.ios_runtimes[0].platform, "iOS");
        assert!(avail.is_ready(), "{:?}", avail.blocking_reason());
    }

    #[test]
    #[cfg(target_arch = "aarch64")]
    fn missing_helper_blocks_readiness_last() {
        let runtimes_json = r#"{"runtimes":[
            {"identifier":"com.apple.CoreSimulator.SimRuntime.iOS-26-3","name":"iOS 26.3","version":"26.3.1","platform":"iOS","isAvailable":true}
        ]}"#;
        let runner = found_xcode_calls(ScriptedRunner::default(), "Xcode 26.3\nBuild version 17C529\n")
            .expect("xcrun simctl list runtimes -j", CmdOutput::ok(runtimes_json))
            .expect("sw_vers -productVersion", CmdOutput::ok("15.7.3\n"));
        let avail = check(&runner, T, &missing_helper, &no_apps);
        assert!(!avail.is_ready());
        assert!(avail.blocking_reason().unwrap().contains("helper"));
    }

    #[test]
    fn the_capture_app_ships_in_contents_helpers() {
        let exe = PathBuf::from("/Applications/OxiMux.app/Contents/MacOS/oximux");
        assert_eq!(
            bundled_capture_path(&exe).unwrap(),
            Path::new("/Applications/OxiMux.app/Contents/Helpers/OxiMux Device Capture.app/Contents/MacOS/oximux-device-capture")
        );
        #[cfg(debug_assertions)]
        assert_eq!(
            dev_capture_path(&PathBuf::from("/repo/target/debug/oximux")).unwrap(),
            Path::new("/repo/target/bundle-tools/OxiMux Device Capture.app/Contents/MacOS/oximux-device-capture")
        );
    }

    #[test]
    #[cfg(debug_assertions)]
    fn bundle_tools_fallback_path_is_target_slash_bundle_tools() {
        let exe = PathBuf::from("/repo/target/debug/oximux");
        let got = bundle_tools_path_from_exe(&exe).unwrap();
        let helper = oximux_sibling_binary::sibling_file_name("oximux-sim-helper"); // `.exe` on Windows
        assert_eq!(got, Path::new("/repo/target").join("bundle-tools").join(helper));
    }

    #[test]
    fn cached_availability_reuses_within_ttl_and_refreshes_after() {
        let cache = CachedAvailability::new(Duration::from_secs(5));
        let start = Instant::now();
        let calls = Mutex::new(0);
        let compute = || {
            *calls.lock().unwrap() += 1;
            Availability {
                xcode: Xcode::Missing { installed: None },
                support: Support::Unsupported("x".into()),
                macos_ok: false,
                arch_ok: false,
                ios_runtimes: Vec::new(),
                helper: missing_helper(),
                verified_xcode: None,
            }
        };

        cache.get(start, compute);
        cache.get(start + Duration::from_secs(4), compute);
        assert_eq!(*calls.lock().unwrap(), 1, "still within the 5s ttl");

        cache.get(start + Duration::from_secs(6), compute);
        assert_eq!(*calls.lock().unwrap(), 2, "ttl elapsed, must recompute");
    }
}
