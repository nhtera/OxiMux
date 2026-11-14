//! `HostConfigRead` / `HostConfigWrite` (v39): the host's own settings.
//!
//! The caps, flags and clocks in [`HostConfig`] reach a running host through a
//! `watch` set when its core was built, so a client that wrote the settings
//! rows directly would change nothing until that host restarted — while
//! reporting success. These two verbs are the way in: the host validates,
//! persists and applies, then says what it enforces.
//!
//! Reading is full scope; writing is the local operator alone. The difference
//! is not caution about a network but about what the keys are: turning
//! `agents.delegation.confine_chat` off hands the next chat agent the
//! operator's own credential instead of a confined one, which is privilege
//! escalation by configuration. See
//! [`AuthStore::may_change_host_config`](crate::auth::AuthStore::may_change_host_config).
//!
//! [`HostConfig`]: crate::HostConfig

use oximux_remote_proto::host_config::HostConfigEntryWire;
use oximux_remote_proto::proto::{Response, RpcError};

use super::Dispatcher;
use crate::auth::Peer;
use crate::host_config::{ConfigEntry, ConfigError};

/// How a refusal reads on the wire. An unknown key and a bad value are the
/// caller's to fix; a host with no settings store has nothing to answer with.
fn refusal(key: &str, err: ConfigError) -> Response {
    let error = match err {
        ConfigError::UnknownKey => RpcError::BadRequest(format!(
            "{key} is not a setting this host has; `oximux config list` names them all"
        )),
        ConfigError::BadValue(takes) => RpcError::BadRequest(format!("{key} {takes}")),
        ConfigError::NoStore => RpcError::Unsupported,
        ConfigError::Write(why) => RpcError::Internal(why),
    };
    Response::Error(error)
}

fn wire(entry: ConfigEntry) -> HostConfigEntryWire {
    HostConfigEntryWire { key: entry.key.to_string(), value: entry.value, is_default: entry.is_default }
}

impl Dispatcher {
    /// One setting, or every one.
    pub(super) fn host_config_read(&self, peer: &Peer, key: Option<&str>) -> Response {
        if !self.auth.may_read_host_config(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(core) = self.host_core.as_ref() else {
            return Response::Error(RpcError::Unsupported);
        };
        match key {
            Some(key) => match core.config_entry(key) {
                Ok(entry) => Response::HostConfig(vec![wire(entry)]),
                Err(err) => refusal(key, err),
            },
            None => match core.config_entries() {
                Ok(entries) => Response::HostConfig(entries.into_iter().map(wire).collect()),
                Err(err) => refusal("", err),
            },
        }
    }

    /// Set one setting and apply it; the reply is what this host now enforces.
    pub(super) fn host_config_write(&self, peer: &Peer, key: &str, value: &str) -> Response {
        if !self.auth.may_change_host_config(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(core) = self.host_core.as_ref() else {
            return Response::Error(RpcError::Unsupported);
        };
        match core.set_config_value(key, value) {
            Ok(entry) => Response::HostConfig(vec![wire(entry)]),
            Err(err) => refusal(key, err),
        }
    }
}

#[cfg(test)]
#[path = "host_config_tests.rs"]
mod tests;
