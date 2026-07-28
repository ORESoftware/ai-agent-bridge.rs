mod tests {
    use super::*;

    #[test]
    fn duplicate_tokens_are_rejected() {
        let json = r#"{
          "credentials": [
            {"token_id":"a","token":"same-token","agent_key":"a","scopes":["workflow:read"]},
            {"token_id":"b","token":"same-token","agent_key":"b","scopes":["workflow:read"]}
          ]
        }"#;
        let error = match WorkflowSecurity::from_json(None, json, 1024) {
            Ok(_) => panic!("duplicate token material must be rejected"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("duplicate workflow credential token material"));
    }

    #[test]
    fn scoped_token_must_not_equal_operator_token() {
        let json = r#"{
          "credentials": [
            {
              "token_id":"adapter-v1",
              "token":"shared-secret",
              "agent_key":"codex",
              "scopes":["workflow:read"]
            }
          ]
        }"#;
        let error = match WorkflowSecurity::from_json(
            Some("shared-secret".into()),
            json,
            1024,
        ) {
            Ok(_) => panic!("operator/scoped token collision must be rejected"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("must not reuse API_AUTH_BEARER material"));
    }

    #[test]
    fn token_lookup_returns_scoped_identity() {
        let json = r#"{
          "credentials": [
            {
              "token_id":"codex-v2",
              "token":"codex-secret",
              "agent_key":"codex",
              "scopes":["workflow:read","workflow:submit"]
            }
          ]
        }"#;
        let security = WorkflowSecurity::from_json(None, json, 1024).unwrap();
        let identity = security.authenticate("codex-secret").unwrap();
        assert_eq!(identity.token_id, "codex-v2");
        assert_eq!(identity.agent_key, "codex");
        assert!(identity.scopes.contains("workflow:submit"));
    }

    #[test]
    fn token_lookup_returns_transport_principals() {
        let json = r#"{
          "credentials": [
            {
              "token_id":"codex-v2",
              "token":"codex-secret",
              "agent_key":"codex",
              "scopes":["channel:read"]
            }
          ]
        }"#;
        let security =
            WorkflowSecurity::from_json(Some("operator-secret".into()), json, 1024).unwrap();
        assert_eq!(
            security.authenticate_principal("operator-secret"),
            Some(AuthenticatedPrincipal::Operator)
        );
        assert!(matches!(
            security.authenticate_principal("codex-secret"),
            Some(AuthenticatedPrincipal::Adapter(identity)) if identity.agent_key == "codex"
        ));
        assert!(security.authenticate_principal("invalid").is_none());
    }

    #[test]
    fn reserved_namespaces_are_detected() {
        assert!(contains_reserved_context_key(Some(&json!({
            "key": "workflow.plan.v1"
        }))));
        assert!(!contains_reserved_context_key(Some(&json!({
            "key": "shared.root-cause"
        }))));
    }
}
