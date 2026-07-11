//! ACP authentication methods ↔ OxiMux's auth card + the worker's authenticate
//! flow.
//!
//! When an ACP agent needs login it fails `session/new`/`session/load` with the
//! JSON-RPC `AuthRequired` code (-32000) and advertises its `auth_methods` at the
//! `initialize` handshake. We translate those methods into the gpui-free
//! [`AuthMethodInfo`] the auth card renders, and detect the -32000 trigger.

use agent_client_protocol::schema::v1::AuthMethod;

use crate::thread::event::{AuthMethodInfo, AuthMethodKind};

/// JSON-RPC error code an ACP agent returns when a session can't open until the
/// client authenticates (`ErrorCode::AuthRequired`, schema `v1/error.rs`).
pub(crate) const AUTH_REQUIRED_CODE: i32 = -32000;

/// Whether an error is the ACP "authentication required" signal (-32000) — the
/// trigger for the auth flow. Any other error stays on the fatal handshake path.
pub(crate) fn is_auth_required(err: &agent_client_protocol::Error) -> bool {
    i32::from(err.code) == AUTH_REQUIRED_CODE
}

/// Convert an ACP `AuthMethod` into the gpui-free [`AuthMethodInfo`] the auth card
/// renders. `EnvVar` carries its variable names (+ optional docs link) for the
/// instructions block; `Terminal` carries the login args the client runs in an
/// embedded terminal; `Agent` (and any future `#[non_exhaustive]` variant)
/// degrades to the plain flow — a bare `authenticate` call.
pub(crate) fn auth_method_info(method: &AuthMethod) -> AuthMethodInfo {
    let kind = match method {
        AuthMethod::EnvVar(e) => AuthMethodKind::EnvVar {
            vars: e.vars.iter().map(|v| v.name.clone()).collect(),
            link: e.link.clone(),
        },
        AuthMethod::Terminal(t) => AuthMethodKind::Terminal {
            args: t.args.clone(),
            env: t.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        },
        _ => AuthMethodKind::Agent,
    };
    AuthMethodInfo {
        id: method.id().0.to_string(),
        name: method.name().to_string(),
        description: method.description().map(str::to_string),
        kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        AuthEnvVar, AuthMethodAgent, AuthMethodEnvVar, AuthMethodTerminal,
    };

    #[test]
    fn auth_required_detects_minus_32000_only() {
        assert!(is_auth_required(&agent_client_protocol::Error::new(-32000, "login first")));
        assert!(!is_auth_required(&agent_client_protocol::Error::new(-32603, "internal")));
        assert!(!is_auth_required(&agent_client_protocol::Error::new(-32601, "no method")));
    }

    #[test]
    fn agent_method_maps_to_agent_kind() {
        let m = AuthMethod::Agent(
            AuthMethodAgent::new("oauth", "Sign in with Google").description("browser login"),
        );
        let info = auth_method_info(&m);
        assert_eq!(info.id, "oauth");
        assert_eq!(info.name, "Sign in with Google");
        assert_eq!(info.description.as_deref(), Some("browser login"));
        assert_eq!(info.kind, AuthMethodKind::Agent);
    }

    #[test]
    fn env_var_method_carries_var_names_and_link() {
        let m = AuthMethod::EnvVar(
            AuthMethodEnvVar::new(
                "api-key",
                "API Key",
                vec![AuthEnvVar::new("ANTHROPIC_API_KEY"), AuthEnvVar::new("OPENAI_API_KEY")],
            )
            .link("https://example.com/keys".to_string()),
        );
        let info = auth_method_info(&m);
        assert_eq!(info.id, "api-key");
        assert_eq!(
            info.kind,
            AuthMethodKind::EnvVar {
                vars: vec!["ANTHROPIC_API_KEY".into(), "OPENAI_API_KEY".into()],
                link: Some("https://example.com/keys".into()),
            }
        );
    }

    #[test]
    fn terminal_method_carries_login_args_and_env() {
        let m = AuthMethod::Terminal(
            AuthMethodTerminal::new("tui", "Terminal Login")
                .args(vec!["auth".into(), "login".into()])
                .env(std::collections::HashMap::from([("CI".to_string(), "1".to_string())])),
        );
        let info = auth_method_info(&m);
        assert_eq!(info.id, "tui");
        assert_eq!(
            info.kind,
            AuthMethodKind::Terminal {
                args: vec!["auth".into(), "login".into()],
                env: vec![("CI".to_string(), "1".to_string())],
            }
        );
    }
}
