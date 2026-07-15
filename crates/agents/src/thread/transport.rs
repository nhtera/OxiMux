//! `Transport` — the runtime tag for which backend a chat connection speaks.
//!
//! Kept in the pure, gpui-free `thread` core (deliberately NOT in
//! `oximux-settings`, which pulls in `gpui` and would pollute this core). It
//! doubles as the persisted `provider` tag on a saved transcript and drives the
//! connection factory ([`super::connect`], added in a later phase). The
//! settings crate has its own TOML-facing `Transport`; the app crate (which
//! depends on both) maps one to the other when it builds a `ConnectSpec`.

use serde::{Deserialize, Serialize};

/// Which backend transport a chat session runs over. `StreamJson` is Claude's
/// native stream-json subprocess — the default, so an old persisted blob that
/// names no provider restores as Claude. `AppServer` drives Codex over its
/// native `codex app-server` JSON-RPC. `Acp` drives an external agent over the
/// Agent Client Protocol. `Rpc` drives Pi over its own newline-JSON `--mode rpc`
/// protocol (Pi speaks neither ACP nor app-server). Serde `lowercase` so the
/// persisted tag is a stable short string (`"streamjson"` / `"appserver"` /
/// `"acp"` / `"rpc"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    #[default]
    StreamJson,
    AppServer,
    Acp,
    Rpc,
}

impl Transport {
    /// Human-facing provider name for chat captions, composer placeholders, and
    /// permission prompts. Derived from the transport so the UI can label a chat
    /// synchronously — before (or without) an established connection, which the
    /// empty state and placeholder both need. `Acp` has no single provider (many
    /// agents share it), so it gets a neutral name until an ACP adapter can thread
    /// a real one.
    pub fn provider_display_name(self) -> &'static str {
        match self {
            Transport::StreamJson => "Claude",
            Transport::AppServer => "Codex",
            Transport::Acp => "Agent",
            Transport::Rpc => "Pi",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_stream_json() {
        assert_eq!(Transport::default(), Transport::StreamJson);
    }

    #[test]
    fn serde_round_trips_lowercase() {
        assert_eq!(serde_json::to_string(&Transport::StreamJson).unwrap(), "\"streamjson\"");
        assert_eq!(serde_json::to_string(&Transport::Acp).unwrap(), "\"acp\"");
        assert_eq!(serde_json::to_string(&Transport::Rpc).unwrap(), "\"rpc\"");
        let acp: Transport = serde_json::from_str("\"acp\"").unwrap();
        assert_eq!(acp, Transport::Acp);
        let rpc: Transport = serde_json::from_str("\"rpc\"").unwrap();
        assert_eq!(rpc, Transport::Rpc);
    }

    #[test]
    fn provider_display_name_is_provider_specific() {
        assert_eq!(Transport::StreamJson.provider_display_name(), "Claude");
        assert_eq!(Transport::AppServer.provider_display_name(), "Codex");
        assert_eq!(Transport::Acp.provider_display_name(), "Agent");
        assert_eq!(Transport::Rpc.provider_display_name(), "Pi");
    }
}
