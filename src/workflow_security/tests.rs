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
        let error = WorkflowSecurity::from_json(None, json, 1024)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("duplicate workflow credential token material"));
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
    fn reserved_namespaces_are_detected() {
        assert!(contains_reserved_context_key(Some(&json!({
            "key": "workflow.plan.v1"
        }))));
        assert!(!contains_reserved_context_key(Some(&json!({
            "key": "shared.root-cause"
        }))));
    }
}
