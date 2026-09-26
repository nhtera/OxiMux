-- V031: which iOS Simulator devices the user has let agents control.
--
-- One row per device (its udid), written only when the user clicks Allow on
-- the desktop's consent banner, and removed when they revoke it in Settings.
-- An approval covers every worktree: it is about the device, not about which
-- project's agent asked first.
--
-- Here rather than in `simulator.toml` on purpose. That file is a settings
-- file an agent's tools edit as a matter of course, so a grant stored there
-- would be one line away from the agent it gates. This database is written only
-- by the app. (Like any guard against a process running as the same user, that
-- is advisory — such a process could also drive the simulator with `xcrun
-- simctl` directly; what consent governs is OxiMux's own verbs.)
--
-- `granted_by` is always 'user' today; it names the writer so a future grant
-- path (an MDM policy, say) is distinguishable rather than indistinguishable.

CREATE TABLE sim_device_approvals (
    udid TEXT PRIMARY KEY NOT NULL,
    device_name TEXT NOT NULL,
    granted_at TEXT NOT NULL,
    granted_by TEXT NOT NULL DEFAULT 'user'
);
