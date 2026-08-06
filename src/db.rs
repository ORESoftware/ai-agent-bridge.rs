//! Optional Postgres persistence (feature = "postgres").
//!
//! This is a best-effort mirror of the in-memory state, never on the request
//! hot path (see [`crate::state::AppState`]'s persistence shims). The canonical
//! DDL is `pg-defs/schema/schema.sql` in the pinned
//! `ORESoftware/k8s-libs-and-shared-defs` submodule. Generated SeaORM entities
//! are adapters only; this service never creates or migrates tables at boot.
//!
//! Most operations remain explicit parameterized PostgreSQL statements because
//! they depend on data-modifying CTEs, window functions, JSONB expressions,
//! server-clock timestamps, or optimistic `EXCLUDED` guards. SeaORM owns the
//! connection, execution, row decoding, and value binding without approximating
//! those semantics through a different query shape.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use sea_orm::sea_query::ArrayType;
use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbBackend, EntityName,
    FromQueryResult, Statement, Value,
};

use crate::state::AppState;
use crate::types::{Agent, ContextEntry, Member, Message};

const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);
const AGENTS_TABLE: &str = "ai_agent_bridge.agents";
const CHANNELS_TABLE: &str = "ai_agent_bridge.channels";
const CHANNEL_MEMBERS_TABLE: &str = "ai_agent_bridge.channel_members";
const MESSAGES_TABLE: &str = "ai_agent_bridge.messages";
const SHARED_CONTEXT_TABLE: &str = "ai_agent_bridge.shared_context";

#[derive(Clone)]
pub struct Db {
    database: DatabaseConnection,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RestoreCounts {
    pub agents: usize,
    pub channels: usize,
    pub messages: usize,
    pub context: usize,
}

#[derive(Debug, FromQueryResult)]
struct AgentRow {
    agent_key: String,
    display_name: String,
    kind: String,
    host: Option<String>,
    meta_data: serde_json::Value,
    registered_at: String,
}

#[derive(Debug, FromQueryResult)]
struct ChannelRow {
    slug: String,
    topic: String,
    embedding_model: String,
    embedding: serde_json::Value,
    created_by: String,
    created_at: String,
    meta_data: serde_json::Value,
}

#[derive(Debug, FromQueryResult)]
struct MessageStatsRow {
    channel_slug: String,
    message_count: i64,
    max_seq: i64,
}

#[derive(Debug, FromQueryResult)]
struct MessageRow {
    id: uuid::Uuid,
    channel_slug: String,
    seq: i64,
    from_agent_key: String,
    role: String,
    content: String,
    meta_data: serde_json::Value,
    created_at: String,
}

#[derive(Debug, FromQueryResult)]
struct ContextRow {
    channel_slug: String,
    ctx_key: String,
    value: serde_json::Value,
    version: i32,
    updated_by: String,
    updated_at: String,
}

impl Db {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        verify_generated_entity_contract();
        let mut options = ConnectOptions::new(url.to_owned());
        options
            .max_connections(5)
            .min_connections(0)
            .acquire_timeout(ACQUIRE_TIMEOUT)
            .sqlx_logging(false);
        let database = Database::connect(options).await?;
        Ok(Self { database })
    }

    /// Restore durable state before listeners accept traffic. Agent metadata,
    /// bounded message history, and shared context survive a restart; channel
    /// membership does not, because presence must be established live.
    pub async fn load_state(&self, state: &Arc<AppState>) -> anyhow::Result<RestoreCounts> {
        let agents = self.load_agents(state).await?;
        let channels = self.load_channels(state).await?;
        let channel_slugs: Vec<String> = state
            .list_channels()
            .into_iter()
            .map(|channel| channel.slug)
            .collect();
        let messages = self.load_messages(state, &channel_slugs).await?;
        let context = self.load_context(state, &channel_slugs).await?;
        Ok(RestoreCounts {
            agents,
            channels,
            messages,
            context,
        })
    }

    async fn load_agents(&self, state: &Arc<AppState>) -> anyhow::Result<usize> {
        let sql = format!(
            "select agent_key, display_name, kind, host, \
             coalesce(meta_data, '{{}}'::jsonb) as meta_data, \
             to_char(created_at at time zone 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') as registered_at \
             from {AGENTS_TABLE} order by updated_at desc, agent_key limit $1"
        );
        let limit = i64::try_from(state.config.max_agents).unwrap_or(i64::MAX);
        let rows = AgentRow::find_by_statement(statement(sql, [limit.into()]))
            .all(&self.database)
            .await?;
        let mut restored = 0;
        for row in rows {
            let agent = Agent {
                agent_key: row.agent_key,
                display_name: row.display_name,
                kind: serde_json::from_value(serde_json::Value::String(row.kind))
                    .unwrap_or_default(),
                host: row.host,
                meta: row.meta_data,
                registered_at: row.registered_at,
            };
            restored += usize::from(state.restore_agent(agent));
        }
        Ok(restored)
    }

    /// Restore channels (topic + embedding) into memory on boot. Returns count.
    pub async fn load_channels(&self, state: &Arc<AppState>) -> anyhow::Result<usize> {
        let sql = format!(
            "select slug, topic, coalesce(embedding_model,'') as embedding_model, \
             embedding, coalesce(created_by,'') as created_by, \
             to_char(created_at at time zone 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') as created_at, \
             coalesce(meta_data, '{{}}'::jsonb) as meta_data \
             from {CHANNELS_TABLE} where status <> 'archived' \
             order by created_at, slug"
        );
        let rows = ChannelRow::find_by_statement(Statement::from_string(DbBackend::Postgres, sql))
            .all(&self.database)
            .await?;
        let mut restored = 0;
        for row in rows {
            if restored >= state.config.max_channels {
                tracing::warn!(
                    loaded = restored,
                    "reached max_channels during restore; skipping the rest"
                );
                break;
            }
            let embedding: Vec<f32> = row
                .embedding
                .as_array()
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_f64().map(|number| number as f32))
                        .collect()
                })
                .unwrap_or_default();
            if state.restore_channel(
                &row.slug,
                &row.topic,
                embedding,
                &row.embedding_model,
                &row.created_by,
                &row.created_at,
                row.meta_data,
            ) {
                restored += 1;
            }
        }
        Ok(restored)
    }

    async fn load_messages(
        &self,
        state: &Arc<AppState>,
        channel_slugs: &[String],
    ) -> anyhow::Result<usize> {
        if channel_slugs.is_empty() {
            return Ok(0);
        }
        let stats_sql = format!(
            "select m.channel_slug, count(*)::bigint as message_count, \
             max(m.seq)::bigint as max_seq \
             from {MESSAGES_TABLE} m \
             where m.channel_slug = any($1::text[]) group by m.channel_slug"
        );
        let stats =
            MessageStatsRow::find_by_statement(statement(stats_sql, [text_array(channel_slugs)]))
                .all(&self.database)
                .await?;
        let mut groups: BTreeMap<String, (Vec<Message>, u64, u64)> = BTreeMap::new();
        for row in stats {
            anyhow::ensure!(
                row.message_count >= 0,
                "negative persisted message count for {}",
                row.channel_slug
            );
            anyhow::ensure!(
                row.max_seq >= 0,
                "negative persisted message sequence for {}",
                row.channel_slug
            );
            groups.insert(
                row.channel_slug,
                (Vec::new(), row.message_count as u64, row.max_seq as u64),
            );
        }

        let messages_sql = format!(
            "select id, channel_slug, seq, from_agent_key, role, content, meta_data, created_at \
             from (select m.id, m.channel_slug, m.seq, m.from_agent_key, m.role, \
                    m.content, coalesce(m.meta_data, '{{}}'::jsonb) as meta_data, \
                    to_char(m.created_at at time zone 'utc', \
                      'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') as created_at, \
                    row_number() over (partition by m.channel_slug order by m.seq desc) as retained_rank \
                   from {MESSAGES_TABLE} m \
                   where m.channel_slug = any($1::text[])) ranked \
             where retained_rank <= $2 and octet_length(content) <= $3 \
               and pg_column_size(meta_data) <= $3 \
             order by channel_slug, seq"
        );
        let history_limit = i64::try_from(state.config.history_limit.max(1)).unwrap_or(i64::MAX);
        let content_limit = i64::try_from(state.config.max_content_bytes).unwrap_or(i64::MAX);
        let rows = MessageRow::find_by_statement(statement(
            messages_sql,
            [
                text_array(channel_slugs),
                history_limit.into(),
                content_limit.into(),
            ],
        ))
        .all(&self.database)
        .await?;
        for row in rows {
            anyhow::ensure!(
                row.seq >= 0,
                "negative persisted message sequence for {}",
                row.channel_slug
            );
            let slug = row.channel_slug;
            let message = Message {
                id: row.id.to_string(),
                channel: slug.clone(),
                seq: row.seq as u64,
                from: row.from_agent_key,
                role: serde_json::from_value(serde_json::Value::String(row.role))
                    .unwrap_or_default(),
                content: row.content,
                meta: row.meta_data,
                created_at: row.created_at,
            };
            if let Some((messages, _, _)) = groups.get_mut(&slug) {
                messages.push(message);
            }
        }
        let mut restored = 0;
        for (slug, (messages, count, max_seq)) in groups {
            let retained = messages.len();
            if state.restore_messages(&slug, messages, count, max_seq) {
                restored += retained;
            }
        }
        Ok(restored)
    }

    async fn load_context(
        &self,
        state: &Arc<AppState>,
        channel_slugs: &[String],
    ) -> anyhow::Result<usize> {
        if channel_slugs.is_empty() {
            return Ok(0);
        }
        let sql = format!(
            "select s.channel_slug, s.ctx_key, s.value, s.version, s.updated_by, \
             to_char(s.updated_at at time zone 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') as updated_at \
             from {SHARED_CONTEXT_TABLE} s \
             where s.channel_slug = any($1::text[]) and pg_column_size(s.value) <= $2 \
             order by s.channel_slug, s.ctx_key"
        );
        let content_limit = i64::try_from(state.config.max_content_bytes).unwrap_or(i64::MAX);
        let rows = ContextRow::find_by_statement(statement(
            sql,
            [text_array(channel_slugs), content_limit.into()],
        ))
        .all(&self.database)
        .await?;
        let mut restored = 0;
        for row in rows {
            anyhow::ensure!(
                row.version >= 0,
                "negative persisted context version for {}",
                row.channel_slug
            );
            let entry = ContextEntry {
                key: row.ctx_key,
                value: row.value,
                version: row.version as u32,
                updated_by: row.updated_by,
                updated_at: row.updated_at,
            };
            restored += usize::from(state.restore_context_entry(&row.channel_slug, entry));
        }
        Ok(restored)
    }

    pub async fn upsert_agent(&self, agent: &Agent) -> anyhow::Result<()> {
        let kind = serde_json::to_value(agent.kind)?
            .as_str()
            .unwrap_or("other")
            .to_string();
        let sql = format!(
            "insert into {AGENTS_TABLE} (agent_key, display_name, kind, host, meta_data) \
             values ($1, $2, $3, $4, $5) \
             on conflict (agent_key) do update set \
               display_name = excluded.display_name, kind = excluded.kind, \
               host = excluded.host, meta_data = excluded.meta_data, updated_at = now()"
        );
        self.execute(statement(
            sql,
            [
                agent.agent_key.clone().into(),
                agent.display_name.clone().into(),
                kind.into(),
                agent.host.clone().into(),
                agent.meta.clone().into(),
            ],
        ))
        .await
    }

    pub async fn upsert_channel(
        &self,
        channel: &crate::types::Channel,
        topic: &str,
        embedding: &[f32],
    ) -> anyhow::Result<()> {
        let embedding_json = serde_json::to_value(embedding)?;
        let dimensions = i32::try_from(embedding.len())
            .map_err(|_| anyhow::anyhow!("embedding dimensions exceed PostgreSQL integer"))?;
        let sql = format!(
            "insert into {CHANNELS_TABLE} \
               (slug, topic, embedding_model, embedding, embedding_dimensions, created_by, meta_data) \
             values ($1, $2, $3, $4, $5, $6, $7) \
             on conflict (slug) do update set \
               topic = excluded.topic, embedding = excluded.embedding, \
               embedding_model = excluded.embedding_model, \
               embedding_dimensions = excluded.embedding_dimensions, updated_at = now()"
        );
        self.execute(statement(
            sql,
            [
                channel.slug.clone().into(),
                topic.to_owned().into(),
                channel.embedding_model.clone().into(),
                embedding_json.into(),
                dimensions.into(),
                channel.created_by.clone().into(),
                channel.meta.clone().into(),
            ],
        ))
        .await
    }

    pub async fn insert_message(&self, message: &Message) -> anyhow::Result<()> {
        let sequence = i64::try_from(message.seq)
            .map_err(|_| anyhow::anyhow!("message sequence exceeds PostgreSQL bigint"))?;
        let role = serde_json::to_value(message.role)?
            .as_str()
            .unwrap_or("user")
            .to_string();
        let sql = format!(
            "insert into {MESSAGES_TABLE} \
               (id, channel_slug, channel_id, seq, from_agent_key, role, content, meta_data) \
             values ($1::uuid, $2, (select id from {CHANNELS_TABLE} where slug = $2), $3, $4, $5, $6, $7) \
             on conflict (channel_slug, seq) do nothing"
        );
        self.execute(statement(
            sql,
            [
                message.id.clone().into(),
                message.channel.clone().into(),
                sequence.into(),
                message.from.clone().into(),
                role.into(),
                message.content.clone().into(),
                message.meta.clone().into(),
            ],
        ))
        .await
    }

    pub async fn upsert_member(&self, slug: &str, member: &Member) -> anyhow::Result<()> {
        let role = serde_json::to_value(member.role)?
            .as_str()
            .unwrap_or("member")
            .to_string();
        let sql = format!(
            "insert into {CHANNEL_MEMBERS_TABLE} \
               (channel_slug, channel_id, agent_key, role) \
             values ($1, (select id from {CHANNELS_TABLE} where slug = $1), $2, $3) \
             on conflict (channel_slug, agent_key) do update set \
               role = excluded.role, last_seen_at = now()"
        );
        self.execute(statement(
            sql,
            [
                slug.to_owned().into(),
                member.agent_key.clone().into(),
                role.into(),
            ],
        ))
        .await
    }

    pub async fn remove_member(&self, slug: &str, agent_key: &str) -> anyhow::Result<()> {
        let sql = format!(
            "delete from {CHANNEL_MEMBERS_TABLE} where channel_slug = $1 and agent_key = $2"
        );
        self.execute(statement(
            sql,
            [slug.to_owned().into(), agent_key.to_owned().into()],
        ))
        .await
    }

    pub async fn save_context(&self, slug: &str, entry: &ContextEntry) -> anyhow::Result<()> {
        let version = i32::try_from(entry.version)
            .map_err(|_| anyhow::anyhow!("context version exceeds PostgreSQL integer"))?;
        let sql = format!(
            "insert into {SHARED_CONTEXT_TABLE} \
               (channel_slug, channel_id, ctx_key, value, version, updated_by) \
             values ($1, (select id from {CHANNELS_TABLE} where slug = $1), $2, $3, $4, $5) \
             on conflict (channel_slug, ctx_key) do update set \
               value = excluded.value, version = excluded.version, \
               updated_by = excluded.updated_by, updated_at = now() \
             where ai_agent_bridge.shared_context.version < excluded.version"
        );
        self.execute(statement(
            sql,
            [
                slug.to_owned().into(),
                entry.key.clone().into(),
                entry.value.clone().into(),
                version.into(),
                entry.updated_by.clone().into(),
            ],
        ))
        .await
    }

    async fn execute(&self, statement: Statement) -> anyhow::Result<()> {
        self.database.execute(statement).await?;
        Ok(())
    }
}

fn statement(sql: String, values: impl IntoIterator<Item = Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

fn text_array(values: &[String]) -> Value {
    Value::Array(
        ArrayType::String,
        Some(Box::new(values.iter().cloned().map(Value::from).collect())),
    )
}

fn verify_generated_entity_contract() {
    use dd_pg_defs_sea_orm::{
        AgentsEntity, ChannelMembersEntity, ChannelsEntity, MessagesEntity, SharedContextEntity,
    };

    for (schema, table) in [
        (AgentsEntity.schema_name(), AgentsEntity.table_name()),
        (ChannelsEntity.schema_name(), ChannelsEntity.table_name()),
        (
            ChannelMembersEntity.schema_name(),
            ChannelMembersEntity.table_name(),
        ),
        (MessagesEntity.schema_name(), MessagesEntity.table_name()),
        (
            SharedContextEntity.schema_name(),
            SharedContextEntity.table_name(),
        ),
    ] {
        debug_assert_eq!(schema, Some("ai_agent_bridge"));
        debug_assert!(!table.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_entity_contract_names_the_expected_schema_and_tables() {
        use dd_pg_defs_sea_orm::{
            AgentsEntity, ChannelMembersEntity, ChannelsEntity, MessagesEntity, SharedContextEntity,
        };

        assert_eq!(AgentsEntity.schema_name(), Some("ai_agent_bridge"));
        assert_eq!(AgentsEntity.table_name(), "agents");
        assert_eq!(ChannelsEntity.table_name(), "channels");
        assert_eq!(ChannelMembersEntity.table_name(), "channel_members");
        assert_eq!(MessagesEntity.table_name(), "messages");
        assert_eq!(SharedContextEntity.table_name(), "shared_context");
    }

    #[test]
    fn text_arrays_remain_bound_postgres_values() {
        let value = text_array(&["a".to_string(), "b".to_string()]);
        assert!(value.is_array());
        assert_eq!(value.as_ref_array().map(Vec::len), Some(2));
    }

    #[test]
    fn complex_statements_keep_placeholders_and_no_interpolation() {
        let advisory = statement(
            format!("select * from {MESSAGES_TABLE} where channel_slug = any($1::text[])"),
            [text_array(&["room".to_string()])],
        );
        assert!(advisory.sql.contains("any($1::text[])"));
        assert!(!advisory.sql.contains("room"));
        assert!(advisory.values.is_some());
    }
}
