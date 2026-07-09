//! Optional Postgres persistence (feature = "postgres").
//!
//! This is a *best-effort mirror* of the in-memory state, never on the request
//! hot path (see [`crate::state::AppState`]'s persist shims, which spawn these).
//! Tables live in the `ai_agent_bridge` schema owned by `remote/libs/pg-defs`;
//! we never create or migrate them here — migrations are human-approved. If the
//! schema has not been applied yet, writes simply error and are logged.
//!
//! Table names come from the generated `dd_pg_defs` contract so this file tracks
//! the canonical schema.

use std::sync::Arc;

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

use crate::state::AppState;
use crate::types::{ContextEntry, Member, Message};

#[derive(Clone)]
pub struct Db {
    pool: PgPool,
}

// Schema-qualified table names, sourced from the pg-defs contract.
use dd_pg_defs::{
    AGENTS_TABLE, CHANNELS_TABLE, CHANNEL_MEMBERS_TABLE, MESSAGES_TABLE, SHARED_CONTEXT_TABLE,
};

impl Db {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(url)
            .await?;
        Ok(Self { pool })
    }

    /// Restore channels (topic + embedding) into memory on boot. Returns count.
    pub async fn load_channels(&self, state: &Arc<AppState>) -> anyhow::Result<usize> {
        let sql = format!(
            "select slug, topic, coalesce(embedding_model,'') as embedding_model, \
             embedding, coalesce(created_by,'') as created_by, \
             to_char(created_at at time zone 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') as created_at, \
             coalesce(meta_data, '{{}}'::jsonb) as meta_data \
             from {CHANNELS_TABLE} where status <> 'archived'"
        );
        let rows = sqlx::query(&sql).fetch_all(&self.pool).await?;
        let mut n = 0;
        for row in rows {
            // Honor the in-memory channel cap even during a boot restore.
            if n >= state.config.max_channels {
                tracing::warn!(loaded = n, "reached max_channels during restore; skipping the rest");
                break;
            }
            let slug: String = row.try_get("slug")?;
            let topic: String = row.try_get("topic").unwrap_or_default();
            let embedding_model: String = row.try_get("embedding_model").unwrap_or_default();
            let created_by: String = row.try_get("created_by").unwrap_or_default();
            let created_at: String = row.try_get("created_at").unwrap_or_default();
            let meta: serde_json::Value = row.try_get("meta_data").unwrap_or_else(|_| serde_json::json!({}));
            let embedding_json: serde_json::Value = row.try_get("embedding").unwrap_or_else(|_| serde_json::json!([]));
            let embedding: Vec<f32> = embedding_json
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect())
                .unwrap_or_default();
            state.restore_channel(&slug, &topic, embedding, &embedding_model, &created_by, &created_at, meta);
            n += 1;
        }
        Ok(n)
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
        let role = serde_json::to_value(msg.role)?.as_str().unwrap_or("user").to_string();
        let sql = format!(
            "insert into {MESSAGES_TABLE} \
               (channel_slug, channel_id, seq, from_agent_key, role, content, meta_data) \
             values ($1, (select id from {CHANNELS_TABLE} where slug = $1), $2, $3, $4, $5, $6) \
             on conflict (channel_slug, seq) do nothing"
        );
        sqlx::query(&sql)
            .bind(&msg.channel)
            .bind(msg.seq as i64)
            .bind(&msg.from)
            .bind(role)
            .bind(&msg.content)
            .bind(&msg.meta)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn upsert_member(&self, slug: &str, member: &Member) -> anyhow::Result<()> {
        let role = serde_json::to_value(member.role)?.as_str().unwrap_or("member").to_string();
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
        let sql = format!("delete from {CHANNEL_MEMBERS_TABLE} where channel_slug = $1 and agent_key = $2");
        sqlx::query(&sql).bind(slug).bind(agent_key).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn save_context(&self, slug: &str, entry: &ContextEntry) -> anyhow::Result<()> {
        let sql = format!(
            "insert into {SHARED_CONTEXT_TABLE} \
               (channel_slug, channel_id, ctx_key, value, version, updated_by) \
             values ($1, (select id from {CHANNELS_TABLE} where slug = $1), $2, $3, $4, $5) \
             on conflict (channel_slug, ctx_key) do update set \
               value = excluded.value, version = excluded.version, \
               updated_by = excluded.updated_by, updated_at = now()"
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
