use super::*;

#[tokio::test]
async fn internal_registry_round_trips_and_tombstones_while_external_context_stays_hidden() {
    let config = crate::config::Config::in_memory();
    let embedder = crate::embed::Embedder::new(
        config.embed_dim,
        None,
        "local".into(),
        None,
        config.max_embedding_response_bytes,
    );
    let state = AppState::new(config, embedder).unwrap();
    let mut descriptor = AuthoritativeLeaseDescriptor {
        version: 1,
        lease_id: "descriptor-test".into(),
        repository: "owner/repo".into(),
        paths: vec!["src/lib.rs".into()],
        agent_key: "codex".into(),
        fencing_token: 42,
        ttl_ms: 30_000,
        acquired_at: now_ts(),
        expires_at: (chrono::Utc::now() + chrono::Duration::minutes(1))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        status: DescriptorStatus::Active,
        released_at: None,
    };

    store_descriptor(&state, &descriptor).await.unwrap();
    assert!(state.get_context(REGISTRY_CHANNEL).unwrap().is_empty());

    let internal = state.get_context_internal(REGISTRY_CHANNEL).unwrap();
    assert_eq!(internal.len(), 1);
    assert_eq!(internal[0].key, descriptor_key(&descriptor.lease_id));
    assert_eq!(internal[0].value["repository"], "owner/repo");

    let loaded = load_active_descriptor(&state, &descriptor.lease_id).unwrap();
    assert_eq!(loaded.repository, descriptor.repository);
    assert_eq!(loaded.paths, descriptor.paths);
    assert_eq!(loaded.fencing_token, descriptor.fencing_token);

    descriptor.status = DescriptorStatus::Released;
    descriptor.released_at = Some(now_ts());
    descriptor.expires_at = now_ts();
    persist_descriptor(&state, &descriptor).unwrap();

    let internal = state.get_context_internal(REGISTRY_CHANNEL).unwrap();
    assert_eq!(internal[0].value["status"], "released");
    assert!(internal[0].value["released_at"].as_str().is_some());
    assert!(load_active_descriptor(&state, &descriptor.lease_id).is_err());
}
