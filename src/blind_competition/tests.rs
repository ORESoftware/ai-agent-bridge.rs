mod tests {
    use super::*;

    #[test]
    fn blind_channel_ids_are_canonical_and_bounded() {
        assert_eq!(blind_channel("abc-123").unwrap(), "blind-workflow-abc-123");
        assert!(blind_channel("../escape").is_err());
        assert!(blind_channel(&"a".repeat(65)).is_err());
    }

    #[test]
    fn agent_keys_reject_control_characters() {
        assert_eq!(normalize_agent_key(" codex ").unwrap(), "codex");
        assert!(normalize_agent_key("codex\nadmin").is_err());
    }
}
