//! Per-connection authorization for the TCP JSONL transport.

use serde_json::{json, Value};

use crate::tcp::Req;
use crate::workflow_security::{AuthenticatedAdapter, AuthenticatedPrincipal, WorkflowSecurity};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TcpPrincipal {
    Unauthenticated,
    Open,
    Operator,
    Adapter(AuthenticatedAdapter),
}

impl TcpPrincipal {
    pub(crate) fn initial(security: &WorkflowSecurity) -> Self {
        if security.authentication_required() {
            Self::Unauthenticated
        } else {
            Self::Open
        }
    }

    pub(crate) fn authenticated(&self) -> bool {
        !matches!(self, Self::Unauthenticated)
    }

    pub(crate) fn authenticate(
        &self,
        security: &WorkflowSecurity,
        token: &str,
    ) -> Result<Self, TcpAccessError> {
        let candidate = if !security.authentication_required() {
            Self::Open
        } else {
            match security.authenticate_principal(token) {
                Some(AuthenticatedPrincipal::Operator) => Self::Operator,
                Some(AuthenticatedPrincipal::Adapter(adapter)) => Self::Adapter(adapter),
                None => return Err(TcpAccessError::new("unauthorized")),
            }
        };

        if self.authenticated() && self != &candidate {
            return Err(TcpAccessError::new("principal_switch_denied"));
        }
        Ok(candidate)
    }

    pub(crate) fn hello_json(&self) -> Value {
        match self {
            Self::Unauthenticated => json!({"authenticated":false}),
            Self::Open => json!({"authenticated":true,"principal":"open"}),
            Self::Operator => json!({"authenticated":true,"principal":"operator"}),
            Self::Adapter(adapter) => json!({
                "authenticated":true,
                "principal":"adapter",
                "agent_key":adapter.agent_key,
            }),
        }
    }

    pub(crate) fn authorize(&self, request: &Req) -> Result<(), TcpAccessError> {
        match self {
            Self::Unauthenticated => Err(TcpAccessError::new("unauthorized")),
            Self::Open | Self::Operator => Ok(()),
            Self::Adapter(adapter) => authorize_adapter(adapter, request),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TcpAccessError {
    code: &'static str,
}

impl TcpAccessError {
    const fn new(code: &'static str) -> Self {
        Self { code }
    }

    pub(crate) fn payload(self) -> Value {
        json!({"ok":false,"error":self.code})
    }
}

fn authorize_adapter(adapter: &AuthenticatedAdapter, request: &Req) -> Result<(), TcpAccessError> {
    let (scope, identity) = match request {
        Req::Auth { .. } | Req::Ping => return Ok(()),
        Req::Register { agent_key, .. } => ("agent:register", Some(agent_key.as_str())),
        Req::ListChannels | Req::Search { .. } => ("channel:read", None),
        Req::CreateChannel { created_by, .. } | Req::Resolve { created_by, .. } => {
            ("channel:create", Some(created_by.as_str()))
        }
        Req::Join { agent_key, .. } | Req::Leave { agent_key, .. } => {
            ("channel:join", Some(agent_key.as_str()))
        }
        Req::Members { .. } | Req::History { .. } => ("channel:read", None),
        Req::Post { from, .. } => ("channel:post", Some(from.as_str())),
        Req::Subscribe { agent_key, .. } => ("channel:read", agent_key.as_deref()),
        Req::GetContext { .. } => ("context:read", None),
        Req::SetContext { updated_by, .. } => ("context:write", Some(updated_by.as_str())),
    };

    if !adapter.has_scope(scope) {
        return Err(TcpAccessError::new("scope_denied"));
    }
    if identity
        .map(str::trim)
        .is_some_and(|identity| identity != adapter.agent_key)
    {
        return Err(TcpAccessError::new("adapter_identity_mismatch"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::json;

    use crate::types::{AgentKind, Role};

    use super::*;

    fn adapter(scopes: &[&str]) -> TcpPrincipal {
        TcpPrincipal::Adapter(AuthenticatedAdapter {
            token_id: "token-1".into(),
            agent_key: "codex".into(),
            scopes: scopes.iter().map(|scope| scope.to_string()).collect(),
        })
    }

    #[test]
    fn adapter_requires_scope_and_matching_identity() {
        let principal = adapter(&["channel:post"]);
        assert!(principal
            .authorize(&Req::Post {
                channel: "room".into(),
                from: "codex".into(),
                content: "hello".into(),
                role: Role::Assistant,
                meta: json!({}),
            })
            .is_ok());
        assert_eq!(
            principal
                .authorize(&Req::Post {
                    channel: "room".into(),
                    from: "claude".into(),
                    content: "spoof".into(),
                    role: Role::Assistant,
                    meta: json!({}),
                })
                .unwrap_err(),
            TcpAccessError::new("adapter_identity_mismatch")
        );
        assert_eq!(
            principal.authorize(&Req::ListChannels).unwrap_err(),
            TcpAccessError::new("scope_denied")
        );
    }

    #[test]
    fn subscription_identity_may_be_implicit_but_not_spoofed() {
        let principal = adapter(&["channel:read"]);
        assert!(principal
            .authorize(&Req::Subscribe {
                channel: "room".into(),
                agent_key: None,
                since: None,
            })
            .is_ok());
        assert!(principal
            .authorize(&Req::Subscribe {
                channel: "room".into(),
                agent_key: Some("codex".into()),
                since: None,
            })
            .is_ok());
        assert!(principal
            .authorize(&Req::Subscribe {
                channel: "room".into(),
                agent_key: Some("other".into()),
                since: None,
            })
            .is_err());
    }

    #[test]
    fn operator_and_open_modes_are_unrestricted() {
        let request = Req::Register {
            agent_key: "any".into(),
            display_name: String::new(),
            kind: AgentKind::Other,
            host: None,
            meta: json!({}),
        };
        assert!(TcpPrincipal::Operator.authorize(&request).is_ok());
        assert!(TcpPrincipal::Open.authorize(&request).is_ok());
    }

    #[test]
    fn principal_hello_never_contains_token_material_or_token_id() {
        let principal = adapter(&["channel:read"]);
        let value = principal.hello_json().to_string();
        assert!(!value.contains("token-1"));
        assert!(!value.contains("secret"));
        assert!(value.contains("codex"));
    }

    #[test]
    fn adapter_scope_set_is_exact() {
        let scopes = BTreeSet::from(["context:read".to_string()]);
        let adapter = AuthenticatedAdapter {
            token_id: "rotating-token".into(),
            agent_key: "codex".into(),
            scopes,
        };
        assert!(adapter.has_scope("context:read"));
        assert!(!adapter.has_scope("context:write"));
    }
}
