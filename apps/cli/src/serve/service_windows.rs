//! `oximux serve` under the Windows Service Control Manager.
//!
//! The Scheduled-Task path (docs/server-install.md) already drains on console
//! signals; this is the real service: `--install-service` registers it,
//! the SCM's `SERVICE_CONTROL_STOP` maps onto the same drain the unix SIGTERM
//! path takes, and a job object guarantees that even a hard-killed service
//! takes its agent children with it — the tree-teardown promise Windows only
//! makes through jobs.
//!
//! Lifecycle: the SCM launches the registered command line
//! (`serve --service --data-dir …`); `main` parses it normally and lands in
//! [`run_service`], which hands control to the service dispatcher. The
//! dispatcher calls [`service_main`] on its own thread; that registers the
//! control handler, reports `RUNNING`, adopts the process into a
//! kill-on-close job, and runs the ordinary serve loop with an injected
//! shutdown channel. `STOP`/`SHUTDOWN` report `STOP_PENDING` with a wait hint
//! comfortably above serve's drain deadline, fire the channel, and the serve
//! loop drains as it would anywhere else; `STOPPED` is reported with its exit
//! code once the drain returns.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_dispatcher;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use super::ServeArgs;
use crate::cli::exit;

/// The SCM's key for the service. Renaming it orphans existing installs —
/// they would need `--uninstall-service` under the old build first.
const SERVICE_NAME: &str = "OxiMuxServe";
const DISPLAY_NAME: &str = "OxiMux Serve";

/// How long a stop may take before the SCM is entitled to lose patience.
/// Comfortably above serve's whole drain path: `DRAIN_DEADLINE` (20 s) +
/// the endpoint join + the 3 s transcript flush, with margin for a slow disk.
const STOP_WAIT_HINT: Duration = Duration::from_secs(45);

/// The parsed args, carried from `main`'s parse (the SCM's registered command
/// line) across the dispatcher's thread hop into [`service_main`] — the
/// dispatcher's own argument vector is the legacy SCM channel and stays empty.
static SERVICE_ARGS: Mutex<Option<ServeArgs>> = Mutex::new(None);

windows_service::define_windows_service!(ffi_service_main, service_main);

/// Entry for `serve --service`: hand this thread to the SCM dispatcher. Only
/// meaningful in a process the SCM launched; run interactively it fails fast
/// with the SCM's own connect error.
pub fn run_service(args: ServeArgs) -> u8 {
    *SERVICE_ARGS.lock().unwrap() = Some(args);
    match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!(
                "serve: --service is for the Service Control Manager, which did not launch \
                 this process ({err}); run plain `oximux serve`, or install with \
                 `oximux serve --install-service`"
            );
            1
        }
    }
}

fn service_main(_scm_args: Vec<OsString>) {
    let Some(args) = SERVICE_ARGS.lock().unwrap().take() else {
        // Unreachable in practice (run_service always stashes first), and
        // with no status handle yet there is nowhere to report — exit.
        return;
    };

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let shutdown_tx = Mutex::new(Some(shutdown_tx));
    // The handler needs the status handle to report STOP_PENDING, but the
    // handle only exists once the handler is registered — a OnceLock breaks
    // the cycle. A control arriving in the register→set window (the SCM does
    // not deliver one before registration returns) still signals shutdown; it
    // only skips the STOP_PENDING report.
    let status_slot: &'static OnceLock<service_control_handler::ServiceStatusHandle> =
        Box::leak(Box::new(OnceLock::new()));

    let handler = move |control: ServiceControl| -> ServiceControlHandlerResult {
        match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                if let Some(handle) = status_slot.get() {
                    let _ = handle.set_service_status(status(
                        ServiceState::StopPending,
                        ServiceControlAccept::empty(),
                        ServiceExitCode::Win32(0),
                        STOP_WAIT_HINT,
                    ));
                }
                if let Some(tx) = shutdown_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let handle = match service_control_handler::register(SERVICE_NAME, handler) {
        Ok(handle) => handle,
        Err(err) => {
            eprintln!("serve: could not register the service control handler: {err}");
            return;
        }
    };
    let _ = status_slot.set(handle);
    let _ = handle.set_service_status(status(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        ServiceExitCode::Win32(0),
        Duration::ZERO,
    ));

    // The hard-stop guarantee: adopt this process into a kill-on-close job,
    // then deliberately leak the handle. It stays open until the process
    // dies — clean exit, `sc stop` overrun, or taskkill — at which point the
    // kernel closes it and terminates every agent child still in the job.
    // Dropping instead of leaking would terminate US at drop time, before the
    // STOPPED report below.
    match oximux_job_object::JobObject::adopt_pid(std::process::id()) {
        Ok(job) => std::mem::forget(job),
        Err(err) => {
            // Serve still works; only the hard-kill teardown guarantee is
            // lost. Say so rather than refusing to start.
            eprintln!("serve: job-object teardown unavailable ({err}); a hard service kill may orphan agent children");
        }
    }

    let code = super::run_with_shutdown(args, Some(shutdown_rx));
    let exit_code = if code == 0 {
        ServiceExitCode::Win32(0)
    } else {
        ServiceExitCode::ServiceSpecific(u32::from(code))
    };
    let _ = handle.set_service_status(status(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        exit_code,
        Duration::ZERO,
    ));
}

fn status(
    current_state: ServiceState,
    controls_accepted: ServiceControlAccept,
    exit_code: ServiceExitCode,
    wait_hint: Duration,
) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state,
        controls_accepted,
        exit_code,
        checkpoint: 0,
        wait_hint,
        process_id: None,
    }
}

/// `oximux serve --install-service`: register the service with the SCM.
///
/// `--data-dir` is REQUIRED here even though plain serve defaults it: the
/// service runs as LocalSystem, whose per-user default resolves to a profile
/// that is not yours — an installed service silently serving an empty data
/// dir would look exactly like data loss.
pub fn install(data_dir: Option<PathBuf>, projects: &[PathBuf]) -> u8 {
    let Some(data_dir) = data_dir else {
        eprintln!(
            "error: --install-service needs an explicit --data-dir — the service runs under \
             the SCM's account, whose default data directory is not yours"
        );
        return exit::USAGE;
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            eprintln!("error: cannot resolve this executable's path: {err}");
            return exit::ERROR;
        }
    };
    let data_dir_display = data_dir.to_string_lossy().into_owned();
    let mut launch_arguments: Vec<OsString> =
        vec!["serve".into(), "--service".into(), "--data-dir".into(), data_dir.into()];
    for project in projects {
        launch_arguments.push("--project".into());
        launch_arguments.push(project.into());
    }

    let outcome = (|| -> Result<(), windows_service::Error> {
        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )?;
        let info = ServiceInfo {
            name: SERVICE_NAME.into(),
            display_name: DISPLAY_NAME.into(),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: exe,
            launch_arguments,
            dependencies: vec![],
            // LocalSystem. Repointing at a user account (for agents that need
            // that user's toolchains and credentials) is an `sc config` /
            // services.msc change after install.
            account_name: None,
            account_password: None,
        };
        let service = manager.create_service(&info, ServiceAccess::CHANGE_CONFIG)?;
        service.set_description(
            "OxiMux headless host: serves agent sessions, terminals, and schedules for \
             the oximux CLI and paired devices.",
        )?;
        Ok(())
    })();

    match outcome {
        Ok(()) => {
            println!(
                "installed service {SERVICE_NAME} (start: automatic)\n\
                 start it now:   sc start {SERVICE_NAME}\n\
                 watch it:       oximux status --dir {data_dir_display}\n\
                 remove it:      oximux serve --uninstall-service"
            );
            exit::OK
        }
        Err(err) => {
            eprintln!(
                "error: could not install the service: {err}\n\
                 (an elevated prompt is required, and an existing {SERVICE_NAME} must be \
                 removed first with --uninstall-service)"
            );
            exit::ERROR
        }
    }
}

/// `oximux serve --uninstall-service`: stop (best-effort) and delete.
pub fn uninstall() -> u8 {
    let outcome = (|| -> Result<(), windows_service::Error> {
        let manager =
            ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let service = manager.open_service(
            SERVICE_NAME,
            ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
        )?;
        // Best-effort: already-stopped answers with an error that must not
        // block the delete.
        let _ = service.stop();
        service.delete()?;
        Ok(())
    })();
    match outcome {
        Ok(()) => {
            println!(
                "removed service {SERVICE_NAME} (deletion completes once the SCM releases it)"
            );
            exit::OK
        }
        Err(err) => {
            eprintln!("error: could not remove the service: {err}");
            exit::ERROR
        }
    }
}

