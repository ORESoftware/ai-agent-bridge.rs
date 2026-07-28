//! Safe-by-default shared-context access.
//!
//! Generic HTTP/TCP/direct callers use the public methods on `AppState` defined
//! here. Reserved workflow and internal namespaces are available only through the
//! explicit crate-internal methods implemented in `state.rs`.

use crate::error::{BridgeError, BridgeResult};
use crate::state::AppState;
use crate::types::ContextEntry;

const RESERVED_CONTEXT_PREFIXES: &[&str] = &["workflow.", "internal."];

impl AppState {
    pub fn set_context(
        &self,
        slug: &str,
        key: &str,
        value: serde_json::Value,
        updated_by: &str,
    ) -> BridgeResult<ContextEntry> {
        ensure_external_context_key(key)?;
        self.set_context_internal(slug, key, value, updated_by)
    }

    pub fn get_context(&self, slug: &str) -> BridgeResult<Vec<ContextEntry>> {
        let mut entries = self.get_context_internal(slug)?;
        entries.retain(|entry| !is_reserved_context_key(&entry.key));
        Ok(entries)
    }

    pub fn get_context_key(&self, slug: &str, key: &str) -> BridgeResult<Option<ContextEntry>> {
        ensure_external_context_key(key)?;
        self.get_context_key_internal(slug, key)
    }
}

pub(crate) fn is_reserved_context_key(key: &str) -> bool {
    let key = key.trim();
    RESERVED_CONTEXT_PREFIXES
        .iter()
        .any(|prefix| key.starts_with(prefix))
}

fn ensure_external_context_key(key: &str) -> BridgeResult<()> {
    if is_reserved_context_key(key) {
        return Err(BridgeError::BadRequest(
            "reserved_context_namespace".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::config::Config;
    use crate::embed::Embedder;

    use super::*;

    async fn state() -> std::sync::Arc<AppState> {
        let config = Config::in_memory();
        let embedder = Embedder::new(
            config.embed_dim,
            None,
            "local".into(),
            None,
            config.max_embedding_response_bytes,
        );
        let state = AppState::new(config, embedder).unwrap();
        state
            .create_or_get_channel("context-test", "context test", "system")
            .await
            .unwrap();
        state
    }

    #[tokio::test]
    async fn generic_context_access_hides_and_rejects_reserved_namespaces() {
        let state = state().await;
        state
            .set_context("context-test", "public.key", json!({"visible":true}), "adapter")
            .unwrap();
        state
            .set_context_internal(
                "context-test",
                "workflow.plan.v1",
                json!({"secret":"internal"}),
                "system",
            )
            .unwrap();

        assert!(state
            .set_context(
                "context-test",
                "workflow.submission.v1.0",
                json!({}),
                "adapter"
            )
            .is_err());
        assert!(state
            .get_context_key("context-test", "workflow.plan.v1")
            .is_err());

        let external = state.get_context("context-test").unwrap();
        assert_eq!(external.len(), 1);
        assert_eq!(external[0].key, "public.key");

        let internal = state.get_context_internal("context-test").unwrap();
        assert_eq!(internal.len(), 2);
        assert!(state
            .get_context_key_internal("context-test", "workflow.plan.v1")
            .unwrap()
            .is_some());
    }
}
