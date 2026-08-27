//! `oximux update` — replace this installation with the latest signed release.
//!
//! Talks to no host: the release server is the only thing it contacts, so the
//! verb runs without a runtime, a socket, or a database. That also means it
//! works on a machine whose `oximux serve` is wedged, which is exactly when
//! someone reaches for it.

use serde_json::{Value, json};

use crate::build_info;
use crate::output::Failure;
use crate::update::{self, Install, download::HttpFetcher};

/// Never printed conditionally on detecting a running host: whether one is up
/// is the service manager's business, and a wrong guess either way is worse
/// than a line that is always true. What the command guarantees is the part
/// that matters — it does not restart anything itself.
const RESTART_HINT: &str =
    "Restart any running `oximux serve` to pick this up; the update never restarts it for you.";

pub fn run(check_only: bool) -> Result<(Value, String), Failure> {
    let fetcher = HttpFetcher;
    let key = build_info::release_public_key();
    let current = build_info::VERSION;

    if check_only {
        let manifest = update::fetch_verified_manifest(&fetcher, key)
            .map_err(update::into_failure)?;
        let newer = update::verify::verify_is_upgrade(&manifest.version, current).is_ok();
        let has_build = manifest.targets.contains_key(build_info::TARGET);
        let data = json!({
            "current": current,
            "latest": manifest.version,
            "channel": manifest.channel,
            "update_available": newer,
            "target": build_info::TARGET,
            "target_available": has_build,
        });
        let human = if !newer {
            format!("oximux {current} is current (latest release: {})", manifest.version)
        } else if has_build {
            format!(
                "oximux {} is available (you have {current}). Run `oximux update` to install it.",
                manifest.version
            )
        } else {
            format!(
                "oximux {} is available, but that release carries no build for {} (it has: {}).",
                manifest.version,
                build_info::TARGET,
                manifest.targets.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        };
        return Ok((data, human));
    }

    // Resolved before the network so "you installed this with Homebrew" comes
    // back immediately rather than after two downloads.
    let install = Install::discover().map_err(update::into_failure)?;
    let applied = update::apply(&fetcher, key, current, build_info::TARGET, &install)
        .map_err(update::into_failure)?;

    let data = json!({
        "updated": true,
        "from": applied.from,
        "to": applied.to,
        "cli": applied_path(&install.cli),
        "relay": applied_path(&install.relay),
        // Non-empty only where a running image cannot be deleted (Windows).
        // Named rather than hidden so a disk-usage question has an answer.
        "pending_cleanup": applied
            .deferred_cleanup
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>(),
    });
    let human = format!(
        "updated oximux {} → {} in {}\n{RESTART_HINT}",
        applied.from,
        applied.to,
        install.dir.display()
    );
    Ok((data, human))
}

fn applied_path(path: &std::path::Path) -> String {
    path.display().to_string()
}

/// `oximux version` — what this build is, and what it can verify.
pub fn version() -> (Value, String) {
    let signed = build_info::release_public_key().is_some();
    let data = json!({
        "version": build_info::VERSION,
        "channel": build_info::CHANNEL,
        "commit": build_info::GIT_SHA,
        "target": build_info::TARGET,
        "protocol_version": oximux_remote_proto::proto::PROTOCOL_VERSION,
        // A build that cannot verify a release cannot self-update. Saying so
        // here means `oximux version` answers "why did update refuse?".
        "self_update": signed,
    });
    let human = format!(
        "oximux {} (protocol v{}){}",
        build_info::describe(),
        oximux_remote_proto::proto::PROTOCOL_VERSION,
        if signed { "" } else { "\nself-update: unavailable (no release key in this build)" }
    );
    (data, human)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `oximux version` is what a bug report pastes, so every build fact has
    /// to be in it — and the honest answer about self-update, since a build
    /// with no key will refuse to update and this is where that is explained.
    #[test]
    fn version_reports_build_channel_and_whether_it_can_self_update() {
        let (data, human) = version();
        assert_eq!(data["version"], build_info::VERSION);
        assert_eq!(data["channel"], build_info::CHANNEL);
        assert_eq!(data["target"], build_info::TARGET);
        assert_eq!(data["commit"], build_info::GIT_SHA);
        assert!(data["protocol_version"].is_number());
        assert_eq!(data["self_update"], build_info::release_public_key().is_some());

        assert!(human.contains(build_info::VERSION));
        assert!(human.contains(build_info::CHANNEL));
        if build_info::release_public_key().is_none() {
            assert!(human.contains("self-update: unavailable"), "{human}");
        }
    }

    /// The one thing this verb promises about a running host.
    #[test]
    fn the_restart_hint_never_claims_to_have_restarted_anything() {
        assert!(RESTART_HINT.contains("never restarts it for you"));
    }
}
