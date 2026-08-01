//! Optional Postgres persistence (feature = "postgres").
//!
//! This is a *best-effort mirror* of the in-memory state, never on the request
//! hot path (see [`crate::state::AppState`]'s persist shims, which spawn these).
//! Tables live in the `ai_agent_bridge` schema defined by `fiducia-interfaces`;
//! we never create or migrate them here — migrations are human-approved. If the
//! schema has not been applied yet, writes simply error and are logged.
//!
//! Constants stay schema-qualified so the service does not depend on a mutable
//! PostgreSQL search path.

use std::collections::BTreeMap;
use std::sync::Arc;

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

use crate::state::AppState;
use crate::types::{Agent, ContextEntry, Member, Message};

#[derive(Clone)]
pub struct Db {
    pool: PgPool,
}

const AGENTS_TABLE: &str = "ai_agent_bridge.agents";
const CHANNELS_TABLE: &str = "ai_agent_bridge.channels";
const CHANNEL_MEMBERS_TABLE: &str = "ai_agent_bridge.channel_members";
const MESSAGES_TABLE: &str = "ai_agent_bridge.messages";
const SHARED_CONTEXT_TABLE: &str = "ai_agent_bridge.shared_context";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RestoreCounts {
    pub agents: usize,
    pub channels: usize,
    pub messages: usize,
    pub context: usize,
}

impl Db {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(url)
            .await?;
        Ok(Self { pool })
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
        let rows = sqlx::query(&sql).bind(limit).fetch_all(&self.pool).await?;
        let mut restored = 0;
        for row in rows {
            let kind: String = row.try_get("kind")?;
            let agent = Agent {
                agent_key: row.try_get("agent_key")?,
                display_name: row.try_get("display_name")?,
                kind: serde_json::from_value(serde_json::Value::String(kind)).unwrap_or_default(),
                host: row.try_get("host")?,
                meta: row.try_get("meta_data")?,
                registered_at: row.try_get("registered_at")?,
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
        let rows = sqlx::query(&sql).fetch_all(&self.pool).await?;
        let mut n = 0;
        for row in rows {
            // Honor the in-memory channel cap even during a boot restore.
            if n >= state.config.max_channels {
                tracing::warn!(
                    loaded = n,
                    "reached max_channels during restore; skipping the rest"
                );
                break;
            }
            let slug: String = row.try_get("slug")?;
            let topic: String = row.try_get("topic").unwrap_or_default();
            let embedding_model: String = row.try_get("embedding_model").unwrap_or_default();
            let created_by: String = row.try_get("created_by").unwrap_or_default();
            let created_at: String = row.try_get("created_at").unwrap_or_default();
            let meta: serde_json::Value = row
                .try_get("meta_data")
                .unwrap_or_else(|_| serde_json::json!({}));
            let embedding_json: serde_json::Value = row
                .try_get("embedding")
                .unwrap_or_else(|_| serde_json::json!([]));
            let embedding: Vec<f32> = embedding_json
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_f64().map(|f| f as f32))
                        .collect()
                })
                .unwrap_or_default();
            if state.restore_channel(
                &slug,
                &topic,
                embedding,
                &embedding_model,
                &created_by,
                &created_at,
                meta,
            ) {
                n += 1;
            }
        }
        Ok(n)
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
        let stats = sqlx::query(&stats_sql)
            .bind(channel_slugs)
            .fetch_all(&self.pool)
            .await?;
        let mut groups: BTreeMap<String, (Vec<Message>, u64, u64)> = BTreeMap::new();
        for row in stats {
            let slug: String = row.try_get("channel_slug")?;
            let count: i64 = row.try_get("message_count")?;
            let max_seq: i64 = row.try_get("max_seq")?;
            anyhow::ensure!(count >= 0, "negative persisted message count for {slug}");
            anyhow::ensure!(
                max_seq >= 0,
                "negative persisted message sequence for {slug}"
            );
            groups.insert(slug, (Vec::new(), count as u64, max_seq as u64));
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
        let rows = sqlx::query(&messages_sql)
            .bind(channel_slugs)
            .bind(history_limit)
            .bind(content_limit)
            .fetch_all(&self.pool)
            .await?;
        for row in rows {
            let slug: String = row.try_get("channel_slug")?;
            let seq: i64 = row.try_get("seq")?;
            anyhow::ensure!(seq >= 0, "negative persisted message sequence for {slug}");
            let role: String = row.try_get("role")?;
            let message = Message {
                id: row.try_get::<uuid::Uuid, _>("id")?.to_string(),
                channel: slug.clone(),
                seq: seq as u64,
                from: row.try_get("from_agent_key")?,
                role: serde_json::from_value(serde_json::Value::String(role)).unwrap_or_default(),
                content: row.try_get("content")?,
                meta: row.try_get("meta_data")?,
                created_at: row.try_get("created_at")?,
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
        let rows = sqlx::query(&sql)
            .bind(channel_slugs)
            .bind(content_limit)
            .fetch_all(&self.pool)
            .await?;
        let mut restored = 0;
        for row in rows {
            let slug: String = row.try_get("channel_slug")?;
            let version: i32 = row.try_get("version")?;
            anyhow::ensure!(
                version >= 0,
                "negative persisted context version for {slug}"
            );
            let entry = ContextEntry {
                key: row.try_get("ctx_key")?,
                value: row.try_get("value")?,
                version: version as u32,
                updated_by: row.try_get("updated_by")?,
                updated_at: row.try_get("updated_at")?,
            };
            restored += usize::from(state.restore_context_entry(&slug, entry));
        }
        Ok(restored)
    }

    pub async fn upsert_agent(&self, agent: &crate::types::Agent) -> anyhow::Result<()> {
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
        sqlx::query(&sql)
            .bind(&agent.agent_key)
            .bind(&agent.display_name)
            .bind(kind)
            .bind(&agent.host)
            .bind(&agent.meta)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn upsert_channel(
        &self,
        channel: &crate::types::Channel,
        topic: &str,
        embedding: &[f32],
    ) -> anyhow::Result<()> {
        let embedding_json = serde_json::to_value(embedding)?;
        let dims = embedding.len() as i32;
        let sql = format!(
            "insert into {CHANNELS_TABLE} \
               (slug, topic, embedding_model, embedding, embedding_dimensions, created_by, meta_data) \
             values ($1, $2, $3, $4, $5, $6, $7) \
             on conflict (slug) do update set \
               topic = excluded.topic, embedding = excluded.embedding, \
               embedding_model = excluded.embedding_model, \
               embedding_dimensions = excluded.embedding_dimensions, updated_at = now()"
        );
        sqlx::query(&sql)
            .bind(&channel.slug)
            .bind(topic)
            .bind(&channel.embedding_model)
            .bind(embedding_json)
            .bind(dims)
            .bind(&channel.created_by)
            .bind(&channel.meta)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn insert_message(&self, msg: &Message) -> anyhow::Result<()> {
        let seq = i64::try_from(msg.seq)
            .map_err(|_| anyhow::anyhow!("message sequence exceeds PostgreSQL bigint"))?;
        let role = serde_json::to_value(msg.role)?
            .as_str()
            .unwrap_or("user")
            .to_string();
        let sql = format!(
            "insert into {MESSAGES_TABLE} \
               (id, channel_slug, channel_id, seq, from_agent_key, role, content, meta_data) \
             values ($1::uuid, $2, (select id from {CHANNELS_TABLE} where slug = $2), $3, $4, $5, $6, $7) \
             on conflict (channel_slug, seq) do nothing"
        );
        sqlx::query(&sql)
            .bind(&msg.id)
            .bind(&msg.channel)
            .bind(seq)
            .bind(&msg.from)
            .bind(role)
            .bind(&msg.content)
            .bind(&msg.meta)
            .execute(&self.pool)
            .await?;
        Ok(())
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
        sqlx::query(&sql)
            .bind(slug)
            .bind(&member.agent_key)
            .bind(role)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn remove_member(&self, slug: &str, agent_key: &str) -> anyhow::Result<()> {
        let sql = format!(
            "delete from {CHANNEL_MEMBERS_TABLE} where channel_slug = $1 and agent_key = $2"
        );
        sqlx::query(&sql)
            .bind(slug)
            .bind(agent_key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn save_context(&self, slug: &str, entry: &ContextEntry) -> anyhow::Result<()> {
        let sql = format!(
            "insert into {SHARED_CONTEXT_TABLE} \
               (channel_slug, channel_id, ctx_key, value, version, updated_by) \
             values ($1, (select id from {CHANNELS_TABLE} where slug = $1), $2, $3, $4, $5) \
             on conflict (channel_slug, ctx_key) do update set \
               value = excluded.value, version = excluded.version, \
               updated_by = excluded.updated_by, updated_at = now() \
             where {SHARED_CONTEXT_TABLE}.version < excluded.version"
        );
        sqlx::query(&sql)
            .bind(slug)
            .bind(&entry.key)
            .bind(&entry.value)
            .bind(entry.version as i32)
            .bind(&entry.updated_by)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
