//! In-memory source of truth for agents, channels, membership, messages, and
//! shared context — plus the live broadcast fan-out that both transports stream
//! from. Postgres (when compiled in) is a best-effort mirror, never on the hot path.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::broadcast;

use crate::config::{Config, MAX_MEMBERS};
use crate::embed::{cosine, Embedder};
use crate::error::{BridgeError, BridgeResult};
use crate::types::*;

/// Max bytes for an `agent_key` (matches the DB `varchar(120)` columns).
const MAX_KEY_BYTES: usize = 120;
/// Max bytes for a shared-context key (matches DB `varchar(200)`).
const MAX_CONTEXT_KEY_BYTES: usize = 200;
/// Max distinct shared-context keys per channel.
const MAX_CONTEXT_KEYS: usize = 10_000;

/// Truncate a `String` to at most `max` bytes without splitting a UTF-8 char.
fn truncate_bytes(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

/// Live per-channel state. The embedding is kept here (never serialized to
/// clients); everything a client sees comes through [`ChannelState::to_public`].
struct ChannelState {
    slug: String,
    topic: String,
    topic_summary: Option<String>,
    embedding: Vec<f32>,
    embedding_model: String,
    created_by: String,
    created_at: Timestamp,
    meta: serde_json::Value,
    members: HashMap<String, Member>,
    messages: VecDeque<Message>,
    next_seq: u64,
    message_count: u64,
    context: HashMap<String, ContextEntry>,
    tx: broadcast::Sender<Event>,
    history_limit: usize,
}

impl ChannelState {
    fn to_public(&self) -> Channel {
        Channel {
            slug: self.slug.clone(),
            topic: self.topic.clone(),
            topic_summary: self.topic_summary.clone(),
            created_by: self.created_by.clone(),
            created_at: self.created_at.clone(),
            member_count: self.members.len(),
            message_count: self.message_count,
            embedding_model: self.embedding_model.clone(),
            meta: self.meta.clone(),
        }
    }
}

#[derive(Debug)]
pub struct ResolveOutcome {
    pub channel: Channel,
    pub score: f32,
    pub created: bool,
}

#[derive(Debug)]
pub struct JoinOutcome {
    pub member: Member,
    pub channel: Channel,
    /// False when the agent was already a member (idempotent re-join).
    pub newly_joined: bool,
}

pub struct AppState {
    pub config: Config,
    pub embedder: Embedder,
    agents: RwLock<HashMap<String, Agent>>,
    channels: RwLock<HashMap<String, ChannelState>>,
    /// Live count of `inbox.jsonl` lines, so `GET /health` is O(1) instead of
    /// re-scanning the whole file on every (unauthenticated) request.
    inbox_count: AtomicU64,
    #[cfg(feature = "postgres")]
    db: Option<crate::db::Db>,
}

impl AppState {
    pub fn new(config: Config, embedder: Embedder) -> Arc<Self> {
        let inbox_count = AtomicU64::new(crate::compat::inbox_count(&config.inbox_dir) as u64);
        Arc::new(Self {
            config,
            embedder,
            agents: RwLock::new(HashMap::new()),
            channels: RwLock::new(HashMap::new()),
            inbox_count,
            #[cfg(feature = "postgres")]
            db: None,
        })
    }

    /// Append a message to the claude-inbox `inbox.jsonl` and bump the counter.
    pub fn append_inbox(&self, msg: &serde_json::Value) -> std::io::Result<()> {
        crate::compat::append_inbox(&self.config.inbox_dir, msg)?;
        self.inbox_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn inbox_message_count(&self) -> u64 {
        self.inbox_count.load(Ordering::Relaxed)
    }

    #[cfg(feature = "postgres")]
    pub fn with_db(mut self: Arc<Self>, db: Option<crate::db::Db>) -> Arc<Self> {
        // Only valid before any clones exist (called once at startup).
        Arc::get_mut(&mut self)
            .expect("with_db must be called before sharing AppState")
            .db = db;
        self
    }

    // ---- agents ---------------------------------------------------------------

    pub fn register_agent(&self, mut agent: Agent) -> BridgeResult<Agent> {
        let key = agent.agent_key.trim().to_string();
        if key.is_empty() {
            return Err(BridgeError::BadRequest("agent_key is required".into()));
        }
        if key.len() > MAX_KEY_BYTES {
            return Err(BridgeError::PayloadTooLarge { what: "agent_key", limit: MAX_KEY_BYTES });
        }
        agent.agent_key = key.clone();
        if agent.display_name.trim().is_empty() {
            agent.display_name = key.clone();
        }
        // Truncate free-text fields to the DB column limits (UTF-8 safe).
        truncate_bytes(&mut agent.display_name, 200);
        if let Some(h) = agent.host.as_mut() {
            truncate_bytes(h, 255);
        }
        agent.registered_at = now_ts();
        {
            let mut agents = self.agents.write();
            if !agents.contains_key(&key) && agents.len() >= self.config.max_agents {
                return Err(BridgeError::CapacityExceeded { what: "agents", limit: self.config.max_agents });
            }
            agents.insert(key, agent.clone());
        }
        self.persist_agent(&agent);
        Ok(agent)
    }

    pub fn list_agents(&self) -> Vec<Agent> {
        self.agents.read().values().cloned().collect()
    }

    // ---- channels -------------------------------------------------------------

    pub fn list_channels(&self) -> Vec<Channel> {
        let mut v: Vec<Channel> = self
            .channels
            .read()
            .values()
            .map(|c| c.to_public())
            .collect();
        v.sort_by(|a, b| a.slug.cmp(&b.slug));
        v
    }

    pub fn get_channel(&self, slug: &str) -> BridgeResult<Channel> {
        self.channels
            .read()
            .get(slug)
            .map(|c| c.to_public())
            .ok_or_else(|| BridgeError::ChannelNotFound(slug.to_string()))
    }

    fn channel_exists(&self, slug: &str) -> bool {
        self.channels.read().contains_key(slug)
    }

    /// Create a channel by slug, or return the existing one unchanged. The topic
    /// is embedded once, on creation.
    pub async fn create_or_get_channel(
        &self,
        slug: &str,
        topic: &str,
        created_by: &str,
    ) -> BridgeResult<Channel> {
        let slug = normalize_slug(slug);
        if slug.is_empty() {
            return Err(BridgeError::BadRequest("slug is required".into()));
        }
        if let Some(ch) = self.channels.read().get(&slug) {
            return Ok(ch.to_public());
        }
        let topic = if topic.trim().is_empty() { slug.replace('-', " ") } else { topic.to_string() };
        let embedding = self.embedder.embed(&topic).await;
        self.insert_channel(slug, topic, created_by, embedding)
    }

    /// Semantic search over topic embeddings, best score first.
    pub async fn search_channels(&self, query: &str, limit: usize) -> Vec<ScoredChannel> {
        let qvec = self.embedder.embed(query).await;
        let mut scored: Vec<ScoredChannel> = {
            let chans = self.channels.read();
            chans
                .values()
                .map(|c| ScoredChannel {
                    channel: c.to_public(),
                    score: cosine(&qvec, &c.embedding),
                })
                .collect()
        };
        scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit.max(1));
        scored
    }

    /// Find the best-matching channel for a query; if none clears `threshold`,
    /// mint a fresh topic. This is the "fluid topic formation" entry point.
    pub async fn resolve_channel(
        &self,
        query: &str,
        created_by: &str,
        threshold: Option<f32>,
    ) -> BridgeResult<ResolveOutcome> {
        let query = query.trim();
        if query.is_empty() {
            return Err(BridgeError::BadRequest("query is required".into()));
        }
        let threshold = threshold.unwrap_or(self.config.resolve_threshold);
        let qvec = self.embedder.embed(query).await;

        let best: Option<(String, f32)> = {
            let chans = self.channels.read();
            chans
                .values()
                .map(|c| (c.slug.clone(), cosine(&qvec, &c.embedding)))
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        };

        if let Some((slug, score)) = best {
            if score >= threshold {
                let channel = self.get_channel(&slug)?;
                return Ok(ResolveOutcome { channel, score, created: false });
            }
        }

        // No sufficiently-close topic — create a new one, reusing the query vector.
        let slug = self.unique_slug(&slugify(query));
        let channel = self.insert_channel(slug, query.to_string(), created_by, qvec)?;
        Ok(ResolveOutcome { channel, score: 0.0, created: true })
    }

    fn insert_channel(
        &self,
        slug: String,
        topic: String,
        created_by: &str,
        embedding: Vec<f32>,
    ) -> BridgeResult<Channel> {
        let public = {
            let mut chans = self.channels.write();
            // Double-checked: a concurrent creator may have won the race.
            if let Some(existing) = chans.get(&slug) {
                return Ok(existing.to_public());
            }
            // Bound total channels so an attacker cannot mint unbounded topics
            // (e.g. via `resolve`/`create`/`POST /claude` with fresh queries).
            if chans.len() >= self.config.max_channels {
                return Err(BridgeError::CapacityExceeded {
                    what: "channels",
                    limit: self.config.max_channels,
                });
            }
            let (tx, _rx) = broadcast::channel(256);
            let created_by = if created_by.trim().is_empty() { "system" } else { created_by };
            let state = ChannelState {
                slug: slug.clone(),
                topic,
                topic_summary: None,
                embedding,
                embedding_model: self.embedder.model_name().to_string(),
                created_by: created_by.to_string(),
                created_at: now_ts(),
                meta: serde_json::json!({}),
                members: HashMap::new(),
                messages: VecDeque::new(),
                next_seq: 1,
                message_count: 0,
                context: HashMap::new(),
                tx,
                history_limit: self.config.history_limit,
            };
            let public = state.to_public();
            chans.insert(slug, state);
            public
        };
        self.persist_channel(&public, &public.topic);
        Ok(public)
    }

    /// Reinstate a channel from persisted state on boot, using its stored
    /// embedding (no re-embedding). Members/messages are not restored — live
    /// chat state is ephemeral; the durable value is the topic + its vector so
    /// semantic routing survives a restart. Idempotent: skips if already present.
    #[cfg(feature = "postgres")]
    pub fn restore_channel(
        &self,
        slug: &str,
        topic: &str,
        embedding: Vec<f32>,
        embedding_model: &str,
        created_by: &str,
        created_at: &str,
        meta: serde_json::Value,
    ) {
        let mut chans = self.channels.write();
        if chans.contains_key(slug) {
            return;
        }
        let (tx, _rx) = broadcast::channel(256);
        chans.insert(
            slug.to_string(),
            ChannelState {
                slug: slug.to_string(),
                topic: topic.to_string(),
                topic_summary: None,
                embedding,
                embedding_model: embedding_model.to_string(),
                created_by: created_by.to_string(),
                created_at: if created_at.is_empty() { now_ts() } else { created_at.to_string() },
                meta,
                members: HashMap::new(),
                messages: VecDeque::new(),
                next_seq: 1,
                message_count: 0,
                context: HashMap::new(),
                tx,
                history_limit: self.config.history_limit,
            },
        );
    }

    fn unique_slug(&self, base: &str) -> String {
        let base = if base.is_empty() { format!("topic-{}", &new_id()[..8]) } else { base.to_string() };
        if !self.channel_exists(&base) {
            return base;
        }
        for n in 2..1000 {
            let candidate = format!("{base}-{n}");
            if !self.channel_exists(&candidate) {
                return candidate;
            }
        }
        format!("{base}-{}", &new_id()[..8])
    }

    // ---- membership -----------------------------------------------------------

    /// Join a chatroom. Idempotent for an existing member. Enforces the 32-seat
    /// cap: the 33rd distinct agent is bounced with [`BridgeError::ChannelFull`].
    pub fn join(&self, slug: &str, agent_key: &str, role: MemberRole) -> BridgeResult<JoinOutcome> {
        let agent_key = agent_key.trim();
        if agent_key.is_empty() {
            return Err(BridgeError::BadRequest("agent_key is required".into()));
        }
        let (outcome, event) = {
            let mut chans = self.channels.write();
            let ch = chans
                .get_mut(slug)
                .ok_or_else(|| BridgeError::ChannelNotFound(slug.to_string()))?;

            if let Some(existing) = ch.members.get_mut(agent_key) {
                existing.last_seen_at = now_ts();
                let member = existing.clone();
                (
                    JoinOutcome { member, channel: ch.to_public(), newly_joined: false },
                    None,
                )
            } else {
                if ch.members.len() >= MAX_MEMBERS {
                    return Err(BridgeError::ChannelFull {
                        slug: slug.to_string(),
                        current: ch.members.len(),
                        limit: MAX_MEMBERS,
                        next: ch.members.len() + 1,
                    });
                }
                let now = now_ts();
                let member = Member {
                    agent_key: agent_key.to_string(),
                    role,
                    joined_at: now.clone(),
                    last_seen_at: now.clone(),
                };
                ch.members.insert(agent_key.to_string(), member.clone());
                let event = Event::Presence {
                    channel: ch.slug.clone(),
                    agent_key: agent_key.to_string(),
                    event: PresenceKind::Joined,
                    member_count: ch.members.len(),
                    at: now,
                };
                (
                    JoinOutcome { member, channel: ch.to_public(), newly_joined: true },
                    Some((ch.tx.clone(), event)),
                )
            }
        };
        if let Some((tx, event)) = event {
            let _ = tx.send(event);
            self.persist_member(slug, &outcome.member);
        }
        Ok(outcome)
    }

    /// Leave a chatroom. Returns whether the agent had been a member.
    pub fn leave(&self, slug: &str, agent_key: &str) -> BridgeResult<bool> {
        let (removed, event) = {
            let mut chans = self.channels.write();
            let ch = chans
                .get_mut(slug)
                .ok_or_else(|| BridgeError::ChannelNotFound(slug.to_string()))?;
            if ch.members.remove(agent_key).is_none() {
                (false, None)
            } else {
                let event = Event::Presence {
                    channel: ch.slug.clone(),
                    agent_key: agent_key.to_string(),
                    event: PresenceKind::Left,
                    member_count: ch.members.len(),
                    at: now_ts(),
                };
                (true, Some((ch.tx.clone(), event)))
            }
        };
        if let Some((tx, event)) = event {
            let _ = tx.send(event);
            self.persist_member_removal(slug, agent_key);
        }
        Ok(removed)
    }

    pub fn members(&self, slug: &str) -> BridgeResult<Vec<Member>> {
        let chans = self.channels.read();
        let ch = chans.get(slug).ok_or_else(|| BridgeError::ChannelNotFound(slug.to_string()))?;
        let mut v: Vec<Member> = ch.members.values().cloned().collect();
        v.sort_by(|a, b| a.joined_at.cmp(&b.joined_at).then(a.agent_key.cmp(&b.agent_key)));
        Ok(v)
    }

    pub fn is_member(&self, slug: &str, agent_key: &str) -> bool {
        self.channels
            .read()
            .get(slug)
            .map(|c| c.members.contains_key(agent_key))
            .unwrap_or(false)
    }

    // ---- messages -------------------------------------------------------------

    /// Post a message. Posting implies membership: a non-member is auto-joined
    /// first (and therefore bounced with `channel_full` if there is no free seat).
    pub fn post_message(
        &self,
        slug: &str,
        from: &str,
        role: Role,
        content: &str,
        meta: serde_json::Value,
    ) -> BridgeResult<Message> {
        let from = from.trim();
        if from.is_empty() {
            return Err(BridgeError::BadRequest("`from` (agent_key) is required".into()));
        }
        if from.len() > MAX_KEY_BYTES {
            return Err(BridgeError::PayloadTooLarge { what: "agent_key", limit: MAX_KEY_BYTES });
        }
        if content.is_empty() {
            return Err(BridgeError::BadRequest("`content` is required".into()));
        }
        if content.len() > self.config.max_content_bytes {
            return Err(BridgeError::PayloadTooLarge {
                what: "message content",
                limit: self.config.max_content_bytes,
            });
        }
        // Ensure a seat (enforces the cap for first-time posters).
        self.join(slug, from, MemberRole::Member)?;

        let (message, tx) = {
            let mut chans = self.channels.write();
            let ch = chans
                .get_mut(slug)
                .ok_or_else(|| BridgeError::ChannelNotFound(slug.to_string()))?;
            let seq = ch.next_seq;
            ch.next_seq += 1;
            ch.message_count += 1;
            if let Some(m) = ch.members.get_mut(from) {
                m.last_seen_at = now_ts();
            }
            let message = Message {
                id: new_id(),
                channel: ch.slug.clone(),
                seq,
                from: from.to_string(),
                role,
                content: content.to_string(),
                meta,
                created_at: now_ts(),
            };
            ch.messages.push_back(message.clone());
            while ch.messages.len() > ch.history_limit {
                ch.messages.pop_front();
            }
            (message, ch.tx.clone())
        };
        let _ = tx.send(Event::Message(message.clone()));
        self.persist_message(&message);
        Ok(message)
    }

    /// Recent messages, optionally only those with `seq > since`.
    pub fn history(&self, slug: &str, since: Option<u64>) -> BridgeResult<Vec<Message>> {
        let chans = self.channels.read();
        let ch = chans.get(slug).ok_or_else(|| BridgeError::ChannelNotFound(slug.to_string()))?;
        let since = since.unwrap_or(0);
        Ok(ch.messages.iter().filter(|m| m.seq > since).cloned().collect())
    }

    /// Subscribe to the live event stream (messages + presence). Auto-joins the
    /// subscriber when `agent_key` is provided (bounced if the room is full).
    pub fn subscribe(
        &self,
        slug: &str,
        agent_key: Option<&str>,
    ) -> BridgeResult<broadcast::Receiver<Event>> {
        if let Some(key) = agent_key.filter(|k| !k.trim().is_empty()) {
            self.join(slug, key, MemberRole::Member)?;
        }
        let chans = self.channels.read();
        let ch = chans.get(slug).ok_or_else(|| BridgeError::ChannelNotFound(slug.to_string()))?;
        Ok(ch.tx.subscribe())
    }

    // ---- shared context -------------------------------------------------------

    pub fn set_context(
        &self,
        slug: &str,
        key: &str,
        value: serde_json::Value,
        updated_by: &str,
    ) -> BridgeResult<ContextEntry> {
        let key = key.trim();
        if key.is_empty() {
            return Err(BridgeError::BadRequest("context key is required".into()));
        }
        if key.len() > MAX_CONTEXT_KEY_BYTES {
            return Err(BridgeError::PayloadTooLarge { what: "context key", limit: MAX_CONTEXT_KEY_BYTES });
        }
        let value_bytes = serde_json::to_vec(&value).map(|v| v.len()).unwrap_or(usize::MAX);
        if value_bytes > self.config.max_content_bytes {
            return Err(BridgeError::PayloadTooLarge {
                what: "context value",
                limit: self.config.max_content_bytes,
            });
        }
        let entry = {
            let mut chans = self.channels.write();
            let ch = chans
                .get_mut(slug)
                .ok_or_else(|| BridgeError::ChannelNotFound(slug.to_string()))?;
            if !ch.context.contains_key(key) && ch.context.len() >= MAX_CONTEXT_KEYS {
                return Err(BridgeError::CapacityExceeded { what: "context keys", limit: MAX_CONTEXT_KEYS });
            }
            let version = ch.context.get(key).map(|e| e.version.saturating_add(1)).unwrap_or(1);
            let entry = ContextEntry {
                key: key.to_string(),
                value,
                version,
                updated_by: updated_by.to_string(),
                updated_at: now_ts(),
            };
            ch.context.insert(key.to_string(), entry.clone());
            entry
        };
        self.persist_context(slug, &entry);
        Ok(entry)
    }

    pub fn get_context(&self, slug: &str) -> BridgeResult<Vec<ContextEntry>> {
        let chans = self.channels.read();
        let ch = chans.get(slug).ok_or_else(|| BridgeError::ChannelNotFound(slug.to_string()))?;
        let mut v: Vec<ContextEntry> = ch.context.values().cloned().collect();
        v.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(v)
    }

    pub fn get_context_key(&self, slug: &str, key: &str) -> BridgeResult<Option<ContextEntry>> {
        let chans = self.channels.read();
        let ch = chans.get(slug).ok_or_else(|| BridgeError::ChannelNotFound(slug.to_string()))?;
        Ok(ch.context.get(key).cloned())
    }

    // ---- persistence shims (no-ops without the `postgres` feature) ------------

    #[cfg(feature = "postgres")]
    fn spawn_persist<F>(&self, fut: impl FnOnce(crate::db::Db) -> F)
    where
        F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        if let Some(db) = &self.db {
            let fut = fut(db.clone());
            tokio::spawn(async move {
                if let Err(e) = fut.await {
                    tracing::warn!(error = %e, "postgres persistence (best-effort) failed");
                }
            });
        }
    }

    #[cfg(feature = "postgres")]
    fn persist_agent(&self, agent: &Agent) {
        let a = agent.clone();
        self.spawn_persist(move |db| async move { db.upsert_agent(&a).await });
    }
    #[cfg(feature = "postgres")]
    fn persist_channel(&self, channel: &Channel, topic: &str) {
        let c = channel.clone();
        let embedding = {
            self.channels.read().get(&channel.slug).map(|s| s.embedding.clone())
        };
        let topic = topic.to_string();
        if let Some(embedding) = embedding {
            self.spawn_persist(move |db| async move { db.upsert_channel(&c, &topic, &embedding).await });
        }
    }
    #[cfg(feature = "postgres")]
    fn persist_member(&self, slug: &str, member: &Member) {
        let (slug, m) = (slug.to_string(), member.clone());
        self.spawn_persist(move |db| async move { db.upsert_member(&slug, &m).await });
    }
    #[cfg(feature = "postgres")]
    fn persist_member_removal(&self, slug: &str, agent_key: &str) {
        let (slug, key) = (slug.to_string(), agent_key.to_string());
        self.spawn_persist(move |db| async move { db.remove_member(&slug, &key).await });
    }
    #[cfg(feature = "postgres")]
    fn persist_message(&self, message: &Message) {
        let m = message.clone();
        self.spawn_persist(move |db| async move { db.insert_message(&m).await });
    }
    #[cfg(feature = "postgres")]
    fn persist_context(&self, slug: &str, entry: &ContextEntry) {
        let (slug, e) = (slug.to_string(), entry.clone());
        self.spawn_persist(move |db| async move { db.save_context(&slug, &e).await });
    }

    // Zero-cost no-ops when Postgres is compiled out.
    #[cfg(not(feature = "postgres"))]
    fn persist_agent(&self, _agent: &Agent) {}
    #[cfg(not(feature = "postgres"))]
    fn persist_channel(&self, _channel: &Channel, _topic: &str) {}
    #[cfg(not(feature = "postgres"))]
    fn persist_member(&self, _slug: &str, _member: &Member) {}
    #[cfg(not(feature = "postgres"))]
    fn persist_member_removal(&self, _slug: &str, _agent_key: &str) {}
    #[cfg(not(feature = "postgres"))]
    fn persist_message(&self, _message: &Message) {}
    #[cfg(not(feature = "postgres"))]
    fn persist_context(&self, _slug: &str, _entry: &ContextEntry) {}
}

/// Lowercase, hyphenate, and trim a free-text topic into a slug.
pub fn slugify(text: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for c in text.trim().to_lowercase().chars() {
        if c.is_alphanumeric() {
            out.push(c);
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.len() > 96 {
        // Byte-safe: `is_alphanumeric()` admits multibyte chars, so a raw
        // truncate(96) could split a char and panic.
        truncate_bytes(&mut out, 96);
        while out.ends_with('-') {
            out.pop();
        }
    }
    out
}

/// Normalize a caller-supplied slug (already slug-ish, but be forgiving).
fn normalize_slug(slug: &str) -> String {
    slugify(slug)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> Arc<AppState> {
        state_cfg(|_| {})
    }

    fn state_cfg(f: impl FnOnce(&mut Config)) -> Arc<AppState> {
        let mut cfg = Config::in_memory();
        f(&mut cfg);
        let embedder = Embedder::new(cfg.embed_dim, "local-hash-v1".into(), None, "local".into(), None);
        AppState::new(cfg, embedder)
    }

    #[test]
    fn truncate_bytes_respects_char_boundaries() {
        // bytes: a(1) é(2) b(1) あ(3) c(1) = 8 total
        let mut s = "aébあc".to_string();
        // max=5 lands inside 'あ' (bytes 4..7); must back off to byte 4.
        truncate_bytes(&mut s, 5);
        assert_eq!(s, "aéb");
        assert_eq!(s.len(), 4);
    }

    #[test]
    fn slugify_does_not_panic_on_long_multibyte() {
        // Unicode letters pass is_alphanumeric() and were pushed raw; a byte-index
        // truncate at 96 could split a char. Must not panic.
        let s = slugify(&"あ".repeat(200));
        assert!(s.len() <= 96);
        assert!(std::str::from_utf8(s.as_bytes()).is_ok());
    }

    #[tokio::test]
    async fn channel_cap_is_enforced() {
        let s = state_cfg(|c| c.max_channels = 2);
        s.create_or_get_channel("a", "topic a", "claude").await.unwrap();
        s.create_or_get_channel("b", "topic b", "claude").await.unwrap();
        // Re-getting an existing channel is fine even at the cap.
        assert!(s.create_or_get_channel("a", "topic a", "claude").await.is_ok());
        // A third distinct channel is rejected.
        let err = s.create_or_get_channel("c", "topic c", "claude").await.unwrap_err();
        assert!(matches!(err, BridgeError::CapacityExceeded { .. }), "got {err:?}");
        // resolve also refuses to mint past the cap.
        let err = s.resolve_channel("something entirely new", "codex", Some(0.99)).await.unwrap_err();
        assert!(matches!(err, BridgeError::CapacityExceeded { .. }));
    }

    #[test]
    fn agent_cap_is_enforced_but_updates_are_free() {
        let s = state_cfg(|c| c.max_agents = 2);
        let mk = |k: &str| Agent { agent_key: k.into(), display_name: String::new(), kind: AgentKind::Other, host: None, meta: serde_json::json!({}), registered_at: String::new() };
        s.register_agent(mk("a")).unwrap();
        s.register_agent(mk("b")).unwrap();
        // Updating an existing agent stays allowed at the cap.
        assert!(s.register_agent(mk("a")).is_ok());
        // A third distinct agent is rejected.
        assert!(matches!(s.register_agent(mk("c")).unwrap_err(), BridgeError::CapacityExceeded { .. }));
    }

    #[test]
    fn agent_key_over_120_bytes_rejected() {
        let s = state();
        let long = "x".repeat(121);
        let a = Agent { agent_key: long, display_name: String::new(), kind: AgentKind::Other, host: None, meta: serde_json::json!({}), registered_at: String::new() };
        assert!(matches!(s.register_agent(a).unwrap_err(), BridgeError::PayloadTooLarge { .. }));
    }

    #[tokio::test]
    async fn oversized_message_and_context_rejected() {
        let s = state_cfg(|c| c.max_content_bytes = 64);
        s.create_or_get_channel("room", "topic", "claude").await.unwrap();
        let big = "z".repeat(65);
        assert!(matches!(
            s.post_message("room", "claude", Role::User, &big, serde_json::json!({})).unwrap_err(),
            BridgeError::PayloadTooLarge { .. }
        ));
        assert!(matches!(
            s.set_context("room", "k", serde_json::json!(big), "claude").unwrap_err(),
            BridgeError::PayloadTooLarge { .. }
        ));
    }

    #[test]
    fn slugify_makes_clean_slugs() {
        assert_eq!(slugify("  Deploy the Soccer Policy!! "), "deploy-the-soccer-policy");
        assert_eq!(slugify("a///b"), "a-b");
        assert!(!slugify("").contains(' '));
    }

    #[tokio::test]
    async fn create_is_idempotent_by_slug() {
        let s = state();
        let a = s.create_or_get_channel("ops", "cluster operations", "claude").await.unwrap();
        let b = s.create_or_get_channel("ops", "something else", "codex").await.unwrap();
        assert_eq!(a.slug, b.slug);
        assert_eq!(a.created_at, b.created_at, "second create must not replace the channel");
        assert_eq!(s.list_channels().len(), 1);
    }

    #[tokio::test]
    async fn join_is_idempotent_and_counts_once() {
        let s = state();
        s.create_or_get_channel("room", "topic", "claude").await.unwrap();
        let first = s.join("room", "claude", MemberRole::Member).unwrap();
        assert!(first.newly_joined);
        let again = s.join("room", "claude", MemberRole::Member).unwrap();
        assert!(!again.newly_joined);
        assert_eq!(s.members("room").unwrap().len(), 1);
    }

    #[tokio::test]
    async fn thirty_third_member_is_bounced() {
        let s = state();
        s.create_or_get_channel("crowded", "topic", "claude").await.unwrap();
        for i in 0..MAX_MEMBERS {
            s.join("crowded", &format!("agent-{i}"), MemberRole::Member).unwrap();
        }
        assert_eq!(s.members("crowded").unwrap().len(), MAX_MEMBERS);
        let err = s.join("crowded", "agent-32", MemberRole::Member).unwrap_err();
        match err {
            BridgeError::ChannelFull { current, limit, next, .. } => {
                assert_eq!(current, 32);
                assert_eq!(limit, 32);
                assert_eq!(next, 33);
            }
            other => panic!("expected ChannelFull, got {other:?}"),
        }
        // Roster unchanged after the bounce.
        assert_eq!(s.members("crowded").unwrap().len(), MAX_MEMBERS);
    }

    #[tokio::test]
    async fn post_assigns_monotonic_seq_and_auto_joins() {
        let s = state();
        s.create_or_get_channel("chat", "topic", "claude").await.unwrap();
        let m1 = s.post_message("chat", "claude", Role::Assistant, "hello", serde_json::json!({})).unwrap();
        let m2 = s.post_message("chat", "codex", Role::Assistant, "hi back", serde_json::json!({})).unwrap();
        assert_eq!(m1.seq, 1);
        assert_eq!(m2.seq, 2);
        // Both posters became members automatically.
        assert_eq!(s.members("chat").unwrap().len(), 2);
    }

    #[tokio::test]
    async fn resolve_reuses_close_topic_and_mints_distant_one() {
        let s = state();
        s.create_or_get_channel("kubernetes-rollouts", "kubernetes deployment rollouts and argocd sync", "claude")
            .await
            .unwrap();
        // Close query resolves to the existing channel.
        let close = s
            .resolve_channel("argocd kubernetes rollout deployment", "codex", Some(0.2))
            .await
            .unwrap();
        assert!(!close.created, "a related query should reuse the topic");
        assert_eq!(close.channel.slug, "kubernetes-rollouts");
        // A distant query with a high threshold mints a new topic.
        let far = s
            .resolve_channel("recipes for sourdough bread", "codex", Some(0.99))
            .await
            .unwrap();
        assert!(far.created, "an unrelated query should form a new topic");
        assert_ne!(far.channel.slug, "kubernetes-rollouts");
    }

    #[tokio::test]
    async fn context_versions_bump() {
        let s = state();
        s.create_or_get_channel("ctx", "topic", "claude").await.unwrap();
        let v1 = s.set_context("ctx", "plan", serde_json::json!({"step": 1}), "claude").unwrap();
        assert_eq!(v1.version, 1);
        let v2 = s.set_context("ctx", "plan", serde_json::json!({"step": 2}), "codex").unwrap();
        assert_eq!(v2.version, 2);
        assert_eq!(s.get_context_key("ctx", "plan").unwrap().unwrap().value, serde_json::json!({"step": 2}));
    }

    #[tokio::test]
    async fn subscribe_receives_posted_message() {
        let s = state();
        s.create_or_get_channel("live", "topic", "claude").await.unwrap();
        // Join before subscribing so no presence event precedes the message.
        s.join("live", "claude", MemberRole::Member).unwrap();
        let mut rx = s.subscribe("live", None).unwrap();
        s.post_message("live", "claude", Role::Assistant, "ping", serde_json::json!({})).unwrap();
        let ev = rx.recv().await.unwrap();
        match ev {
            Event::Message(m) => assert_eq!(m.content, "ping"),
            other => panic!("expected message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn leave_emits_presence_and_updates_count() {
        let s = state();
        s.create_or_get_channel("presence", "topic", "claude").await.unwrap();
        let mut rx = s.subscribe("presence", None).unwrap();
        s.join("presence", "codex", MemberRole::Member).unwrap();
        // First event is the join presence.
        match rx.recv().await.unwrap() {
            Event::Presence { event, member_count, .. } => {
                assert_eq!(event, PresenceKind::Joined);
                assert_eq!(member_count, 1);
            }
            other => panic!("expected join presence, got {other:?}"),
        }
        s.leave("presence", "codex").unwrap();
        match rx.recv().await.unwrap() {
            Event::Presence { event, member_count, .. } => {
                assert_eq!(event, PresenceKind::Left);
                assert_eq!(member_count, 0);
            }
            other => panic!("expected leave presence, got {other:?}"),
        }
    }
}
