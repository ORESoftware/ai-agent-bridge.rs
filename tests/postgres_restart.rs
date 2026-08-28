#![cfg(feature = "postgres")]

use ai_agent_bridge::config::Config;
use ai_agent_bridge::db::Db;
use ai_agent_bridge::embed::Embedder;
use ai_agent_bridge::state::AppState;
use ai_agent_bridge::types::{Agent, AgentKind, Role};
use sea_orm::{ConnectionTrait, Database, DbBackend, FromQueryResult, Statement};

const RESET_SQL: &str = r#"
truncate table ai_agent_bridge.channel_members, ai_agent_bridge.messages,
  ai_agent_bridge.shared_context, ai_agent_bridge.channels,
  ai_agent_bridge.agents cascade
"#;

#[derive(Debug, FromQueryResult)]
struct MessageCountRow {
    count: i64,
    max_seq: i64,
}

fn state_with_db(db: Db) -> std::sync::Arc<AppState> {
    let config = Config::in_memory();
    let embedder = Embedder::new(
        config.embed_dim,
        None,
        "local-hash-v1".into(),
        None,
        config.max_embedding_response_bytes,
    );
    AppState::new(config, embedder)
        .expect("create app state")
        .with_db(Some(db))
}

#[tokio::test]
#[ignore = "requires FIDUCIA_BRIDGE_TEST_DATABASE_URL provisioned from canonical pg-defs schema.sql"]
async fn restart_restores_history_context_and_agent_metadata_without_stale_presence() {
    let database_url = std::env::var("FIDUCIA_BRIDGE_TEST_DATABASE_URL")
        .expect("FIDUCIA_BRIDGE_TEST_DATABASE_URL must name a dedicated canonical-schema database");
    let setup = Database::connect(&database_url)
        .await
        .expect("connect test database through SeaORM");
    setup
        .execute(Statement::from_string(
            DbBackend::Postgres,
            RESET_SQL.to_owned(),
        ))
        .await
        .expect("reset isolated bridge tables");

    let db = Db::connect(&database_url).await.expect("connect bridge DB");
    let original = state_with_db(db.clone());
    original
        .register_agent(Agent {
            agent_key: "codex-restart".into(),
            display_name: "Codex Restart Witness".into(),
            kind: AgentKind::Codex,
            host: Some("test-host".into()),
            meta: serde_json::json!({"durable": true}),
            registered_at: String::new(),
        })
        .expect("register agent");
    original
        .create_or_get_channel("restart-room", "restart durability", "codex-restart")
        .await
        .expect("create channel");
    let first = original
        .post_message(
            "restart-room",
            "codex-restart",
            Role::Assistant,
            "first durable message",
            serde_json::json!({"ordinal": 1}),
        )
        .expect("post first message");
    let second = original
        .post_message(
            "restart-room",
            "codex-restart",
            Role::Assistant,
            "second durable message",
            serde_json::json!({"ordinal": 2}),
        )
        .expect("post second message");
    let older_context = original
        .set_context(
            "restart-room",
            "decision",
            serde_json::json!({"next": "prepare restart"}),
            "codex-restart",
        )
        .expect("save context");
    let context = original
        .set_context(
            "restart-room",
            "decision",
            serde_json::json!({"next": "test restart"}),
            "codex-restart",
        )
        .expect("update context");
    original.flush_persistence().await;
    db.save_context("restart-room", &older_context)
        .await
        .expect("late stale context write is harmless");
    drop(original);

    let restored = state_with_db(db.clone());
    let counts = db.load_state(&restored).await.expect("restore state");
    assert_eq!(counts.agents, 1);
    assert_eq!(counts.channels, 1);
    assert_eq!(counts.messages, 2);
    assert_eq!(counts.context, 1);

    let agents = restored.list_agents();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].agent_key, "codex-restart");
    assert_eq!(agents[0].meta, serde_json::json!({"durable": true}));
    let channel = restored
        .get_channel("restart-room")
        .expect("restored channel");
    assert_eq!(
        channel.member_count, 0,
        "presence must be live, not durable"
    );
    assert_eq!(channel.message_count, 2);
    assert!(restored
        .members("restart-room")
        .expect("restored membership")
        .is_empty());

    let history = restored.history("restart-room", None).expect("history");
    assert_eq!(
        history
            .iter()
            .map(|message| message.seq)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(history[0].id, first.id, "message identity survives restart");
    assert_eq!(
        history[1].id, second.id,
        "message identity survives restart"
    );
    let restored_context = restored
        .get_context_key("restart-room", "decision")
        .expect("context lookup")
        .expect("restored context");
    assert_eq!(restored_context.key, context.key);
    assert_eq!(restored_context.value, context.value);
    assert_eq!(restored_context.version, context.version);
    assert_eq!(restored_context.updated_by, context.updated_by);

    let third = restored
        .post_message(
            "restart-room",
            "codex-restart",
            Role::Assistant,
            "third message after restart",
            serde_json::json!({"ordinal": 3}),
        )
        .expect("post after restart");
    assert_eq!(
        third.seq, 3,
        "sequence must resume above the durable high-water"
    );
    restored.flush_persistence().await;

    let row = MessageCountRow::find_by_statement(Statement::from_string(
        DbBackend::Postgres,
        "select count(*)::bigint as count, max(seq)::bigint as max_seq \
         from ai_agent_bridge.messages where channel_slug = 'restart-room'"
            .to_owned(),
    ))
    .one(&setup)
    .await
    .expect("query durable messages")
    .expect("count row");
    assert_eq!(row.count, 3);
    assert_eq!(row.max_seq, 3);
}
