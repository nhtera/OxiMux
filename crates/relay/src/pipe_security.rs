//! Owner-only access control for the Windows named pipe.
//!
//! A unix-domain socket inherits the protection of the directory holding it, so
//! the relay's socket is unreachable to other accounts without anything being
//! spelled out. `\\.\pipe\` has no such containment: it is one flat namespace
//! shared by every session on the machine, and a pipe created with the default
//! security descriptor is reachable by principals that have no business
//! carrying this daemon's traffic — which is every keystroke typed into every
//! terminal and every byte those terminals print back.
//!
//! So the descriptor is stated explicitly, and every path through this module
//! either produces one or fails. There is deliberately no fallback that creates
//! the pipe without it: a relay that refuses to start is a visible problem,
//! while a relay listening on an open pipe is an invisible one.
//!
//! The descriptor itself comes from `oximux-owner-only`, which the token and
//! key files use too — the pipe and those files are protected by one definition
//! of "only this account", not three that can drift apart.

use anyhow::{Context, Result};
use interprocess::os::windows::security_descriptor::SecurityDescriptor;
use widestring::U16CString;

/// Build a security descriptor granting the current user full control and
/// nobody else any access at all.
pub fn owner_only_descriptor() -> Result<SecurityDescriptor> {
    let sddl = oximux_owner_only::owner_only_sddl().context("build owner-only SDDL")?;
    let wide =
        U16CString::from_str(&sddl).context("security descriptor string is not valid UTF-16")?;
    // Rejects a malformed descriptor here rather than at pipe creation, which is
    // why a mistake upstream surfaces as a startup failure.
    SecurityDescriptor::deserialize(&wide)
        .with_context(|| format!("parse security descriptor {sddl}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptor_builds_for_the_running_user() {
        // Exercises the SDDL end to end: if the shared builder ever emits
        // something `interprocess` cannot parse, this fails rather than a pipe
        // silently opening wide.
        assert!(owner_only_descriptor().is_ok());
    }
}
