//! In-memory source of truth for agents, channels, membership, messages, and
//! shared context — plus the live broadcast fan-out that both transports stream
//! from. Postgres (when compiled in) is a best-effort mirror, never on the hot path.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
/// Max bytes for a channel topic (matches the DB `octet_length` CHECK).
const MAX_TOPIC_BYTES: usize = 8192;
const MAX_REPOSITORY_BYTES: usize = 512;
const MAX_FILE_PATH_BYTES: usize = 4096;
const MAX_FILE_PURPOSE_BYTES: usize = 1024;
const MIN_FILE_LEASE_TTL_MS: u64 = 100;
const MAX_FILE_LEASE_TTL_MS: u64 = 3_600_000;

/// Retained size of a message for the per-channel byte budget: content + its
/// serialized `meta` (the variable-size fields that actually accumulate), so a
/// content-empty / meta-heavy message can't slip the budget.
fn message_retained_bytes(m: &Message) -> usize {
    m.content.len() + serde_json::to_vec(&m.meta).map(|v| v.len()).unwrap_or(0)
}

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
    /// Running sum of retained message `content` bytes, to bound per-channel
    /// memory by size (not just count) — one hot channel can't hoard ~1 GiB.
    history_bytes: usize,
    next_seq: u64,
    message_count: u64,
    context: HashMap<String, ContextEntry>,
    tx: broadcast::Sender<Event>,
    history_limit: usize,
}

struct FileLeaseState {
    lease: FileLease,
    expires_at: Instant,
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
    pub control_plane: Option<crate::control_plane::ControlPlaneClient>,
    /// Bounds concurrent HTTP SSE streams (each holds a broadcast receiver + a
    /// forwarder task). The TCP listener has its own connection semaphore; this
    /// is the HTTP-side equivalent so live streams can't grow without bound.
    pub sse_connections: Arc<tokio::sync::Semaphore>,
    agents: RwLock<HashMap<String, Agent>>,
    file_leases: RwLock<HashMap<String, FileLeaseState>>,
    next_file_fencing_token: AtomicU64,
    channels: RwLock<HashMap<String, ChannelState>>,
    /// Live count of `inbox.jsonl` lines, so `GET /health` is O(1) instead of
    /// re-scanning the whole file on every (unauthenticated) request.
    inbox_count: AtomicU64,
    #[cfg(feature = "postgres")]
    db: Option<crate::db::Db>,
    /// Bounds concurrent best-effort persist tasks; excess writes are shed so a
    /// write flood can't spawn unbounded tasks queued on the DB pool.
    #[cfg(feature = "postgres")]
    persist_sem: Arc<tokio::sync::Semaphore>,
}

impl AppState {
    pub fn new(config: Config, embedder: Embedder) -> std::io::Result<Arc<Self>> {
        crate::compat::prepare_inbox(&config.inbox_dir)?;
        let inbox_count = AtomicU64::new(crate::compat::inbox_count(&config.inbox_dir)? as u64);
        let control_plane = crate::control_plane::ControlPlaneClient::from_config(&config);
        let sse_connections = Arc::new(tokio::sync::Semaphore::new(config.max_sse_connections));
        Ok(Arc::new(Self {
            config,
            embedder,
            control_plane,
            sse_connections,
            agents: RwLock::new(HashMap::new()),
            file_leases: RwLock::new(HashMap::new()),
            next_file_fencing_token: AtomicU64::new(0),
            channels: RwLock::new(HashMap::new()),
            inbox_count,
            #[cfg(feature = "postgres")]
            db: None,
            #[cfg(feature = "postgres")]
            persist_sem: Arc::new(tokio::sync::Semaphore::new(256)),
        }))
    }

    /// Append a message to the claude-inbox `inbox.jsonl` and bump the counter.
    pub fn append_inbox<T: serde::Serialize>(&self, msg: &T) -> std::io::Result<()> {
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
            return Err(BridgeError::PayloadTooLarge {
                what: "agent_key",
                limit: MAX_KEY_BYTES,
            });
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
        let meta_bytes = serde_json::to_vec(&agent.meta)
            .map(|v| v.len())
            .unwrap_or(usize::MAX);
        if meta_bytes > self.config.max_content_bytes {
            return Err(BridgeError::PayloadTooLarge {
                what: "agent meta",
                limit: self.config.max_content_bytes,
            });
        }
        agent.registered_at = now_ts();
        {
            let mut agents = self.agents.write();
            if !agents.contains_key(&key) && agents.len() >= self.config.max_agents {
                return Err(BridgeError::CapacityExceeded {
                    what: "agents",
                    limit: self.config.max_agents,
                });
            }
            agents.insert(key, agent.clone());
        }
        self.persist_agent(&agent);
        Ok(agent)
    }

    pub fn list_agents(&self) -> Vec<Agent> {
        self.agents.read().values().cloned().collect()
    }

    pub fn get_agent(&self, agent_key: &str) -> Option<Agent> {
        self.agents.read().get(agent_key).cloned()
    }

    // ---- repository path leases ---------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn acquire_file_lease(
        &self,
        repository: &str,
        path: &str,
        agent_key: &str,
        ttl_ms: u64,
        recursive: bool,
        purpose: &str,
        meta: serde_json::Value,
    ) -> BridgeResult<FileLease> {
        let repository = normalize_repository(repository)?;
        let path = normalize_file_path(path)?;
        let agent_key = agent_key.trim().to_string();
        if !self.agents.read().contains_key(&agent_key) {
            return Err(BridgeError::AgentNotFound(agent_key));
        }
        let meta_bytes = serde_json::to_vec(&meta)
            .map(|value| value.len())
            .unwrap_or(usize::MAX);
        if meta_bytes > self.config.max_content_bytes {
            return Err(BridgeError::PayloadTooLarge {
                what: "file lease meta",
                limit: self.config.max_content_bytes,
            });
        }
        let ttl_ms = normalize_file_lease_ttl(ttl_ms)?;
        let now = Instant::now();
        let mut leases = self.file_leases.write();
        leases.retain(|_, value| value.expires_at > now);

        if let Some(conflict) = leases.values().find(|value| {
            value.lease.repository == repository
                && value.lease.agent_key != agent_key
                && lease_scopes_overlap(&value.lease.path, value.lease.recursive, &path, recursive)
        }) {
            return Err(BridgeError::FileLeaseConflict {
                repository,
                path,
                holder: conflict.lease.agent_key.clone(),
            });
        }

        if let Some(existing) = leases.values_mut().find(|value| {
            value.lease.repository == repository
                && value.lease.path == path
                && value.lease.agent_key == agent_key
        }) {
            existing.expires_at = now + Duration::from_millis(ttl_ms);
            existing.lease.expires_at = file_lease_expiry_ts(ttl_ms);
            existing.lease.recursive = recursive;
            existing.lease.purpose = normalized_purpose(purpose);
            existing.lease.meta = meta;
            return Ok(existing.lease.clone());
        }

        if leases.len() >= self.config.max_file_leases {
            return Err(BridgeError::CapacityExceeded {
                what: "file leases",
                limit: self.config.max_file_leases,
            });
        }
        let lease = FileLease {
            id: new_id(),
            repository,
            path,
            recursive,
            agent_key,
            purpose: normalized_purpose(purpose),
            meta,
            fencing_token: self
                .next_file_fencing_token
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1),
            acquired_at: now_ts(),
            expires_at: file_lease_expiry_ts(ttl_ms),
        };
        leases.insert(
            lease.id.clone(),
            FileLeaseState {
                lease: lease.clone(),
                expires_at: now + Duration::from_millis(ttl_ms),
            },
        );
        Ok(lease)
    }

    pub fn renew_file_lease(
        &self,
        lease_id: &str,
        agent_key: &str,
        fencing_token: u64,
        ttl_ms: u64,
    ) -> BridgeResult<FileLease> {
        let ttl_ms = normalize_file_lease_ttl(ttl_ms)?;
        let now = Instant::now();
        let mut leases = self.file_leases.write();
        leases.retain(|_, value| value.expires_at > now);
        let value = leases
            .get_mut(lease_id)
            .ok_or_else(|| BridgeError::FileLeaseNotFound(lease_id.to_string()))?;
        if value.lease.agent_key != agent_key.trim() {
            return Err(BridgeError::FileLeaseOwnerMismatch {
                lease_id: lease_id.to_string(),
                agent: agent_key.trim().to_string(),
            });
        }
        if value.lease.fencing_token != fencing_token {
            return Err(BridgeError::StaleFencingToken(lease_id.to_string()));
        }
        value.expires_at = now + Duration::from_millis(ttl_ms);
        value.lease.expires_at = file_lease_expiry_ts(ttl_ms);
        Ok(value.lease.clone())
    }

    pub fn release_file_lease(
        &self,
        lease_id: &str,
        agent_key: &str,
        fencing_token: u64,
    ) -> BridgeResult<FileLease> {
        let now = Instant::now();
        let mut leases = self.file_leases.write();
        leases.retain(|_, value| value.expires_at > now);
        let value = leases
            .get(lease_id)
            .ok_or_else(|| BridgeError::FileLeaseNotFound(lease_id.to_string()))?;
        if value.lease.agent_key != agent_key.trim() {
            return Err(BridgeError::FileLeaseOwnerMismatch {
                lease_id: lease_id.to_string(),
                agent: agent_key.trim().to_string(),
            });
        }
        if value.lease.fencing_token != fencing_token {
            return Err(BridgeError::StaleFencingToken(lease_id.to_string()));
        }
        Ok(leases
            .remove(lease_id)
            .expect("validated file lease exists")
            .lease)
    }

    pub fn file_lease_holders(
        &self,
        repository: Option<&str>,
        path: Option<&str>,
        agent_key: Option<&str>,
        include_descendants: bool,
    ) -> BridgeResult<Vec<FileLeaseHolder>> {
        let repository = repository.map(normalize_repository).transpose()?;
        let path = path.map(normalize_file_path).transpose()?;
        let now = Instant::now();
        let mut leases = self.file_leases.write();
        leases.retain(|_, value| value.expires_at > now);
        let agents = self.agents.read();
        let mut holders: Vec<FileLeaseHolder> = leases
            .values()
            .filter(|value| {
                repository
                    .as_ref()
                    .is_none_or(|repo| value.lease.repository == *repo)
                    && path.as_ref().is_none_or(|query| {
                        lease_scopes_overlap(
                            &value.lease.path,
                            value.lease.recursive,
                            query,
                            include_descendants,
                        )
                    })
                    && agent_key.is_none_or(|agent| value.lease.agent_key == agent)
            })
            .filter_map(|value| {
                agents
                    .get(&value.lease.agent_key)
                    .cloned()
                    .map(|agent| FileLeaseHolder {
                        lease: value.lease.clone(),
                        agent,
                    })
            })
            .collect();
        holders.sort_by(|a, b| {
            (&a.lease.repository, &a.lease.path, &a.lease.agent_key).cmp(&(
                &b.lease.repository,
                &b.lease.path,
                &b.lease.agent_key,
            ))
        });
        Ok(holders)
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
        let topic = if topic.trim().is_empty() {
            slug.replace('-', " ")
        } else {
            topic.to_string()
        };
        let (embedding, model) = self.embedder.embed(&topic).await;
        Ok(self
            .insert_channel(slug, topic, created_by, embedding, model)?
            .0)
    }

    /// Semantic search over topic embeddings, best score first.
    pub async fn search_channels(&self, query: &str, limit: usize) -> Vec<ScoredChannel> {
        let (qvec, _) = self.embedder.embed(query).await;
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
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
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
        // A NaN threshold makes every `score >= threshold` false (mint until
        // capacity); out-of-range distorts routing. Require finite, clamp to [0,1].
        let threshold = threshold
            .filter(|t| t.is_finite())
            .map(|t| t.clamp(0.0, 1.0))
            .unwrap_or(self.config.resolve_threshold);
        let (qvec, model) = self.embedder.embed(query).await;

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
                return Ok(ResolveOutcome {
                    channel,
                    score,
                    created: false,
                });
            }
        }

        // No sufficiently-close topic — create a new one, reusing the query vector.
        let slug = self.unique_slug(&slugify(query));
        let (channel, created) =
            self.insert_channel(slug, query.to_string(), created_by, qvec, model)?;
        Ok(ResolveOutcome {
            channel,
            score: 0.0,
            created,
        })
    }

    /// Returns `(channel, created)` — `created` is false when a concurrent creator
    /// won the race (the caller gets the existing channel), so `resolve` reports
    /// the truth instead of always claiming `created: true`.
    fn insert_channel(
        &self,
        slug: String,
        mut topic: String,
        created_by: &str,
        embedding: Vec<f32>,
        embedding_model: String,
    ) -> BridgeResult<(Channel, bool)> {
        truncate_bytes(&mut topic, MAX_TOPIC_BYTES);
        let mut created_by = created_by.trim().to_string();
        if created_by.is_empty() {
            created_by = "system".to_string();
        }
        truncate_bytes(&mut created_by, MAX_KEY_BYTES);
        let (public, created) = {
            let mut chans = self.channels.write();
            // Double-checked: a concurrent creator may have won the race.
            if let Some(existing) = chans.get(&slug) {
                return Ok((existing.to_public(), false));
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
            let state = ChannelState {
                slug: slug.clone(),
                topic,
                topic_summary: None,
                embedding,
                embedding_model,
                created_by,
                created_at: now_ts(),
                meta: serde_json::json!({}),
                members: HashMap::new(),
                messages: VecDeque::new(),
                history_bytes: 0,
                next_seq: 1,
                message_count: 0,
                context: HashMap::new(),
                tx,
                history_limit: self.config.history_limit,
            };
            let public = state.to_public();
            chans.insert(slug, state);
            (public, true)
        };
        if created {
            self.persist_channel(&public, &public.topic);
        }
        Ok((public, created))
    }

    /// Reinstate a channel from persisted state on boot, using its stored
    /// embedding (no re-embedding). Members/messages are not restored — live
    /// chat state is ephemeral; the durable value is the topic + its vector so
    /// semantic routing survives a restart. Idempotent: skips if already present.
    #[cfg(feature = "postgres")]
    #[allow(clippy::too_many_arguments)] // Mirrors one complete persisted channel row.
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
                created_at: if created_at.is_empty() {
                    now_ts()
                } else {
                    created_at.to_string()
                },
                meta,
                members: HashMap::new(),
                messages: VecDeque::new(),
                history_bytes: 0,
                next_seq: 1,
                message_count: 0,
                context: HashMap::new(),
                tx,
                history_limit: self.config.history_limit,
            },
        );
    }

    fn unique_slug(&self, base: &str) -> String {
        let base = if base.is_empty() {
            format!("topic-{}", &new_id()[..8])
        } else {
            base.to_string()
        };
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
        // Same cap as register/post so join/subscribe can't smuggle an oversized
        // key into membership + presence broadcasts (and break PG persistence).
        if agent_key.len() > MAX_KEY_BYTES {
            return Err(BridgeError::PayloadTooLarge {
                what: "agent_key",
                limit: MAX_KEY_BYTES,
            });
        }
        let (outcome, persist) = {
            let mut chans = self.channels.write();
            let ch = chans
                .get_mut(slug)
                .ok_or_else(|| BridgeError::ChannelNotFound(slug.to_string()))?;

            if let Some(existing) = ch.members.get_mut(agent_key) {
                existing.last_seen_at = now_ts();
                let member = existing.clone();
                (
                    JoinOutcome {
                        member,
                        channel: ch.to_public(),
                        newly_joined: false,
                    },
                    false,
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
                // Broadcast presence under the write lock (like messages) so a join
                // is causally ordered before any message that follows it.
                let _ = ch.tx.send(Event::Presence {
                    channel: ch.slug.clone(),
                    agent_key: agent_key.to_string(),
                    event: PresenceKind::Joined,
                    member_count: ch.members.len(),
                    at: now,
                });
                (
                    JoinOutcome {
                        member,
                        channel: ch.to_public(),
                        newly_joined: true,
                    },
                    true,
                )
            }
        };
        if persist {
            self.persist_member(slug, &outcome.member);
        }
        Ok(outcome)
    }

    /// Leave a chatroom. Returns whether the agent had been a member.
    pub fn leave(&self, slug: &str, agent_key: &str) -> BridgeResult<bool> {
        let removed = {
            let mut chans = self.channels.write();
            let ch = chans
                .get_mut(slug)
                .ok_or_else(|| BridgeError::ChannelNotFound(slug.to_string()))?;
            if ch.members.remove(agent_key).is_none() {
                false
            } else {
                // Under the lock, like join/messages, for causal ordering.
                let _ = ch.tx.send(Event::Presence {
                    channel: ch.slug.clone(),
                    agent_key: agent_key.to_string(),
                    event: PresenceKind::Left,
                    member_count: ch.members.len(),
                    at: now_ts(),
                });
                true
            }
        };
        if removed {
            self.persist_member_removal(slug, agent_key);
        }
        Ok(removed)
    }

    pub fn members(&self, slug: &str) -> BridgeResult<Vec<Member>> {
        let chans = self.channels.read();
        let ch = chans
            .get(slug)
            .ok_or_else(|| BridgeError::ChannelNotFound(slug.to_string()))?;
        let mut v: Vec<Member> = ch.members.values().cloned().collect();
        v.sort_by(|a, b| {
            a.joined_at
                .cmp(&b.joined_at)
                .then(a.agent_key.cmp(&b.agent_key))
        });
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
            return Err(BridgeError::BadRequest(
                "`from` (agent_key) is required".into(),
            ));
        }
        if from.len() > MAX_KEY_BYTES {
            return Err(BridgeError::PayloadTooLarge {
                what: "agent_key",
                limit: MAX_KEY_BYTES,
            });
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
        // Cap `meta` too, else 1 byte of content + megabytes of JSON meta bypasses
        // the content cap and accumulates in the history ring.
        let meta_bytes = serde_json::to_vec(&meta)
            .map(|v| v.len())
            .unwrap_or(usize::MAX);
        if meta_bytes > self.config.max_content_bytes {
            return Err(BridgeError::PayloadTooLarge {
                what: "message meta",
                limit: self.config.max_content_bytes,
            });
        }
        // Ensure a seat (enforces the cap for first-time posters).
        self.join(slug, from, MemberRole::Member)?;

        let budget = self.config.max_channel_history_bytes;
        let message = {
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
            ch.history_bytes += message_retained_bytes(&message);
            ch.messages.push_back(message.clone());
            // Evict oldest by count AND by total retained bytes (always keep >= 1),
            // so one hot channel can't hoard ~1 GiB of history.
            while ch.messages.len() > ch.history_limit
                || (ch.history_bytes > budget && ch.messages.len() > 1)
            {
                if let Some(old) = ch.messages.pop_front() {
                    ch.history_bytes = ch
                        .history_bytes
                        .saturating_sub(message_retained_bytes(&old));
                }
            }
            // Broadcast under the write lock so live delivery order matches `seq`
            // (two concurrent posts can't be observed out of order). send is
            // non-blocking, so holding the lock across it is safe and brief.
            let _ = ch.tx.send(Event::Message(message.clone()));
            message
        };
        self.persist_message(&message);
        Ok(message)
    }

    /// Recent messages, optionally only those with `seq > since`.
    pub fn history(&self, slug: &str, since: Option<u64>) -> BridgeResult<Vec<Message>> {
        let chans = self.channels.read();
        let ch = chans
            .get(slug)
            .ok_or_else(|| BridgeError::ChannelNotFound(slug.to_string()))?;
        let since = since.unwrap_or(0);
        Ok(ch
            .messages
            .iter()
            .filter(|m| m.seq > since)
            .cloned()
            .collect())
    }

    /// Subscribe to the live event stream (messages + presence). Auto-joins the
    /// subscriber when `agent_key` is provided (bounced if the room is full).
    ///
    /// Returns `(receiver, high_water)`: the receiver only yields messages posted
    /// after this call (seq > high_water), and `high_water` is the last seq that
    /// already existed. A caller replaying history should replay only up to
    /// `high_water`, so a message in the subscribe window is not delivered twice
    /// (once as replay, once live). The receiver + high_water are captured under
    /// the same read lock, and posts broadcast under the write lock, so the split
    /// is exact — no duplicate, no gap.
    pub fn subscribe(
        &self,
        slug: &str,
        agent_key: Option<&str>,
    ) -> BridgeResult<(broadcast::Receiver<Event>, u64)> {
        if let Some(key) = agent_key.filter(|k| !k.trim().is_empty()) {
            self.join(slug, key, MemberRole::Member)?;
        }
        let chans = self.channels.read();
        let ch = chans
            .get(slug)
            .ok_or_else(|| BridgeError::ChannelNotFound(slug.to_string()))?;
        let rx = ch.tx.subscribe();
        let high_water = ch.next_seq.saturating_sub(1);
        Ok((rx, high_water))
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
            return Err(BridgeError::PayloadTooLarge {
                what: "context key",
                limit: MAX_CONTEXT_KEY_BYTES,
            });
        }
        let value_bytes = serde_json::to_vec(&value)
            .map(|v| v.len())
            .unwrap_or(usize::MAX);
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
                return Err(BridgeError::CapacityExceeded {
                    what: "context keys",
                    limit: MAX_CONTEXT_KEYS,
                });
            }
            let version = ch
                .context
                .get(key)
                .map(|e| e.version.saturating_add(1))
                .unwrap_or(1);
            let mut updated_by = updated_by.to_string();
            truncate_bytes(&mut updated_by, MAX_KEY_BYTES);
            let entry = ContextEntry {
                key: key.to_string(),
                value,
                version,
                updated_by,
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
        let ch = chans
            .get(slug)
            .ok_or_else(|| BridgeError::ChannelNotFound(slug.to_string()))?;
        let mut v: Vec<ContextEntry> = ch.context.values().cloned().collect();
        v.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(v)
    }

    pub fn get_context_key(&self, slug: &str, key: &str) -> BridgeResult<Option<ContextEntry>> {
        let chans = self.channels.read();
        let ch = chans
            .get(slug)
            .ok_or_else(|| BridgeError::ChannelNotFound(slug.to_string()))?;
        Ok(ch.context.get(key).cloned())
    }

    // ---- persistence shims (no-ops without the `postgres` feature) ------------

    #[cfg(feature = "postgres")]
    fn spawn_persist<F>(&self, fut: impl FnOnce(crate::db::Db) -> F)
    where
        F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        if let Some(db) = &self.db {
            // Shed (drop) the write under overload rather than queue an unbounded
            // task on the 5-connection pool — this is a best-effort mirror.
            let permit = match self.persist_sem.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    tracing::debug!("persist queue full; dropping a best-effort write");
                    return;
                }
            };
            let fut = fut(db.clone());
            tokio::spawn(async move {
                let _permit = permit;
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
            self.channels
                .read()
                .get(&channel.slug)
                .map(|s| s.embedding.clone())
        };
        let topic = topic.to_string();
        if let Some(embedding) = embedding {
            self.spawn_persist(
                move |db| async move { db.upsert_channel(&c, &topic, &embedding).await },
            );
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

fn normalize_repository(value: &str) -> BridgeResult<String> {
    let value = value.trim().trim_end_matches('/');
    if value.is_empty() {
        return Err(BridgeError::BadRequest("repository is required".into()));
    }
    if value.len() > MAX_REPOSITORY_BYTES {
        return Err(BridgeError::PayloadTooLarge {
            what: "repository",
            limit: MAX_REPOSITORY_BYTES,
        });
    }
    if value.chars().any(char::is_control) {
        return Err(BridgeError::BadRequest(
            "repository contains control characters".into(),
        ));
    }
    Ok(value.to_string())
}

fn normalize_file_path(value: &str) -> BridgeResult<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(BridgeError::BadRequest("path is required".into()));
    }
    if value.len() > MAX_FILE_PATH_BYTES {
        return Err(BridgeError::PayloadTooLarge {
            what: "file path",
            limit: MAX_FILE_PATH_BYTES,
        });
    }
    if value.starts_with('/') || value.contains('\\') || value.chars().any(char::is_control) {
        return Err(BridgeError::BadRequest(
            "path must be a repository-relative POSIX path".into(),
        ));
    }
    let mut parts = Vec::new();
    for part in value.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                return Err(BridgeError::BadRequest(
                    "path must not traverse outside the repository".into(),
                ))
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return Err(BridgeError::BadRequest("path is required".into()));
    }
    Ok(parts.join("/"))
}

fn normalize_file_lease_ttl(ttl_ms: u64) -> BridgeResult<u64> {
    if !(MIN_FILE_LEASE_TTL_MS..=MAX_FILE_LEASE_TTL_MS).contains(&ttl_ms) {
        return Err(BridgeError::BadRequest(format!(
            "ttl_ms must be between {MIN_FILE_LEASE_TTL_MS} and {MAX_FILE_LEASE_TTL_MS}"
        )));
    }
    Ok(ttl_ms)
}

fn normalized_purpose(value: &str) -> String {
    let mut value = value.trim().to_string();
    truncate_bytes(&mut value, MAX_FILE_PURPOSE_BYTES);
    value
}

fn file_lease_expiry_ts(ttl_ms: u64) -> Timestamp {
    (chrono::Utc::now() + chrono::Duration::milliseconds(ttl_ms as i64))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn lease_scopes_overlap(a_path: &str, a_recursive: bool, b_path: &str, b_recursive: bool) -> bool {
    a_path == b_path
        || (a_recursive && b_path.starts_with(&format!("{a_path}/")))
        || (b_recursive && a_path.starts_with(&format!("{b_path}/")))
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
        let embedder = Embedder::new(
            cfg.embed_dim,
            None,
            "local".into(),
            None,
            cfg.max_embedding_response_bytes,
        );
        AppState::new(cfg, embedder).unwrap()
    }

    // Exercises the file-lease coordination the control-plane proxies to (an
    // agent leases a repo path before editing it; the fencing token authorizes
    // renew/release; another agent is blocked while it is held).
    #[test]
    fn file_lease_acquire_conflict_fence_and_lookup() {
        let s = state();
        let mk = |k: &str| Agent {
            agent_key: k.into(),
            display_name: String::new(),
            kind: AgentKind::Other,
            host: None,
            meta: serde_json::json!({}),
            registered_at: String::new(),
        };
        s.register_agent(mk("alice")).unwrap();
        s.register_agent(mk("bob")).unwrap();

        let acquire = |agent: &str, path: &str| {
            s.acquire_file_lease(
                "acme/api",
                path,
                agent,
                30_000,
                false,
                "edit",
                serde_json::json!({}),
            )
        };

        // A lease for an unregistered agent is rejected.
        assert!(matches!(
            acquire("ghost", "src/lib.rs"),
            Err(BridgeError::AgentNotFound(_))
        ));

        // Alice leases src/lib.rs and receives a fencing token.
        let lease = acquire("alice", "src/lib.rs").unwrap();
        assert_eq!(lease.agent_key, "alice");
        assert!(lease.fencing_token >= 1);

        // Bob is blocked on the same path, but free to lease a different one.
        assert!(matches!(
            acquire("bob", "src/lib.rs"),
            Err(BridgeError::FileLeaseConflict { .. })
        ));
        acquire("bob", "src/other.rs").unwrap();

        // Lookups: who holds a path, and what an agent holds.
        assert_eq!(
            s.file_lease_holders(Some("acme/api"), Some("src/lib.rs"), None, false)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            s.file_lease_holders(None, None, Some("bob"), false)
                .unwrap()
                .len(),
            1
        );

        // Renew/release are fenced: wrong token or wrong owner is refused.
        assert!(matches!(
            s.renew_file_lease(&lease.id, "alice", lease.fencing_token + 1, 30_000),
            Err(BridgeError::StaleFencingToken(_))
        ));
        assert!(matches!(
            s.renew_file_lease(&lease.id, "bob", lease.fencing_token, 30_000),
            Err(BridgeError::FileLeaseOwnerMismatch { .. })
        ));
        s.renew_file_lease(&lease.id, "alice", lease.fencing_token, 30_000)
            .unwrap();

        // Releasing frees the path for the previously-blocked agent.
        s.release_file_lease(&lease.id, "alice", lease.fencing_token)
            .unwrap();
        acquire("bob", "src/lib.rs").unwrap();
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
        s.create_or_get_channel("a", "topic a", "claude")
            .await
            .unwrap();
        s.create_or_get_channel("b", "topic b", "claude")
            .await
            .unwrap();
        // Re-getting an existing channel is fine even at the cap.
        assert!(s
            .create_or_get_channel("a", "topic a", "claude")
            .await
            .is_ok());
        // A third distinct channel is rejected.
        let err = s
            .create_or_get_channel("c", "topic c", "claude")
            .await
            .unwrap_err();
        assert!(
            matches!(err, BridgeError::CapacityExceeded { .. }),
            "got {err:?}"
        );
        // resolve also refuses to mint past the cap.
        let err = s
            .resolve_channel("something entirely new", "codex", Some(0.99))
            .await
            .unwrap_err();
        assert!(matches!(err, BridgeError::CapacityExceeded { .. }));
    }

    #[test]
    fn agent_cap_is_enforced_but_updates_are_free() {
        let s = state_cfg(|c| c.max_agents = 2);
        let mk = |k: &str| Agent {
            agent_key: k.into(),
            display_name: String::new(),
            kind: AgentKind::Other,
            host: None,
            meta: serde_json::json!({}),
            registered_at: String::new(),
        };
        s.register_agent(mk("a")).unwrap();
        s.register_agent(mk("b")).unwrap();
        // Updating an existing agent stays allowed at the cap.
        assert!(s.register_agent(mk("a")).is_ok());
        // A third distinct agent is rejected.
        assert!(matches!(
            s.register_agent(mk("c")).unwrap_err(),
            BridgeError::CapacityExceeded { .. }
        ));
    }

    #[test]
    fn agent_key_over_120_bytes_rejected() {
        let s = state();
        let long = "x".repeat(121);
        let a = Agent {
            agent_key: long,
            display_name: String::new(),
            kind: AgentKind::Other,
            host: None,
            meta: serde_json::json!({}),
            registered_at: String::new(),
        };
        assert!(matches!(
            s.register_agent(a).unwrap_err(),
            BridgeError::PayloadTooLarge { .. }
        ));
    }

    #[tokio::test]
    async fn oversized_message_and_context_rejected() {
        let s = state_cfg(|c| c.max_content_bytes = 64);
        s.create_or_get_channel("room", "topic", "claude")
            .await
            .unwrap();
        let big = "z".repeat(65);
        assert!(matches!(
            s.post_message("room", "claude", Role::User, &big, serde_json::json!({}))
                .unwrap_err(),
            BridgeError::PayloadTooLarge { .. }
        ));
        assert!(matches!(
            s.set_context("room", "k", serde_json::json!(big), "claude")
                .unwrap_err(),
            BridgeError::PayloadTooLarge { .. }
        ));
    }

    // ---- round-3 review fixes ------------------------------------------------

    #[tokio::test]
    async fn join_rejects_oversized_key() {
        let s = state();
        s.create_or_get_channel("room", "topic", "claude")
            .await
            .unwrap();
        let long = "k".repeat(121);
        assert!(matches!(
            s.join("room", &long, MemberRole::Member).unwrap_err(),
            BridgeError::PayloadTooLarge { .. }
        ));
    }

    #[tokio::test]
    async fn post_rejects_oversized_meta() {
        let s = state_cfg(|c| c.max_content_bytes = 128);
        s.create_or_get_channel("room", "topic", "claude")
            .await
            .unwrap();
        let big_meta = serde_json::json!({ "blob": "m".repeat(200) });
        assert!(matches!(
            s.post_message("room", "claude", Role::User, "hi", big_meta)
                .unwrap_err(),
            BridgeError::PayloadTooLarge { .. }
        ));
    }

    #[tokio::test]
    async fn resolve_reports_created_honestly_and_reuses() {
        let s = state();
        let first = s
            .resolve_channel("brand new topic here", "claude", Some(0.99))
            .await
            .unwrap();
        assert!(first.created);
        // A near-identical query resolves back to the same channel, created=false.
        let again = s
            .resolve_channel("brand new topic here", "codex", Some(0.2))
            .await
            .unwrap();
        assert!(!again.created);
        assert_eq!(again.channel.slug, first.channel.slug);
    }

    #[tokio::test]
    async fn resolve_ignores_nan_threshold() {
        let s = state();
        s.create_or_get_channel("existing", "kubernetes rollout deploy", "claude")
            .await
            .unwrap();
        // A NaN threshold must not make every comparison false (which would mint a
        // new channel); it falls back to the configured default and reuses.
        let out = s
            .resolve_channel("kubernetes rollout deploy", "codex", Some(f32::NAN))
            .await
            .unwrap();
        assert!(
            !out.created,
            "NaN threshold should fall back, not force-create"
        );
        assert_eq!(out.channel.slug, "existing");
    }

    #[tokio::test]
    async fn local_embedding_model_label_is_honest() {
        let s = state();
        let ch = s
            .create_or_get_channel("room", "a topic", "claude")
            .await
            .unwrap();
        assert_eq!(ch.embedding_model, crate::embed::LOCAL_MODEL);
    }

    #[tokio::test]
    async fn subscribe_high_water_splits_replay_and_live() {
        let s = state();
        s.create_or_get_channel("c", "t", "claude").await.unwrap();
        s.post_message("c", "claude", Role::User, "old", serde_json::json!({}))
            .unwrap(); // seq 1
        let (mut rx, hw) = s.subscribe("c", None).unwrap();
        assert_eq!(hw, 1, "high-water is the last existing seq");
        s.post_message("c", "claude", Role::User, "new", serde_json::json!({}))
            .unwrap(); // seq 2
                       // The live receiver yields ONLY seq > high_water (no duplicate of the replay).
        match rx.recv().await.unwrap() {
            Event::Message(m) => {
                assert_eq!(m.seq, 2);
                assert_eq!(m.content, "new");
            }
            other => panic!("expected the new message, got {other:?}"),
        }
        // Replay is exactly seq <= high_water — the two sets partition cleanly.
        let replay: Vec<_> = s
            .history("c", None)
            .unwrap()
            .into_iter()
            .filter(|m| m.seq <= hw)
            .collect();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].content, "old");
    }

    #[tokio::test]
    async fn history_is_bounded_by_bytes_not_just_count() {
        // Tiny byte budget, generous count limit -> eviction is driven by bytes.
        let s = state_cfg(|c| {
            c.max_channel_history_bytes = 120;
            c.history_limit = 10_000;
        });
        s.create_or_get_channel("room", "topic", "claude")
            .await
            .unwrap();
        for i in 0..50 {
            s.post_message(
                "room",
                "claude",
                Role::User,
                &format!("message-{i:03}-payload"),
                serde_json::json!({}),
            )
            .unwrap();
        }
        let hist = s.history("room", None).unwrap();
        // Each message is ~20 bytes; a 120-byte budget keeps only a handful.
        assert!(
            hist.len() <= 8,
            "history should be byte-bounded, got {}",
            hist.len()
        );
        // The newest message is always retained.
        assert_eq!(hist.last().unwrap().content, "message-049-payload");
    }

    #[test]
    fn slugify_makes_clean_slugs() {
        assert_eq!(
            slugify("  Deploy the Soccer Policy!! "),
            "deploy-the-soccer-policy"
        );
        assert_eq!(slugify("a///b"), "a-b");
        assert!(!slugify("").contains(' '));
    }

    #[tokio::test]
    async fn create_is_idempotent_by_slug() {
        let s = state();
        let a = s
            .create_or_get_channel("ops", "cluster operations", "claude")
            .await
            .unwrap();
        let b = s
            .create_or_get_channel("ops", "something else", "codex")
            .await
            .unwrap();
        assert_eq!(a.slug, b.slug);
        assert_eq!(
            a.created_at, b.created_at,
            "second create must not replace the channel"
        );
        assert_eq!(s.list_channels().len(), 1);
    }

    #[tokio::test]
    async fn join_is_idempotent_and_counts_once() {
        let s = state();
        s.create_or_get_channel("room", "topic", "claude")
            .await
            .unwrap();
        let first = s.join("room", "claude", MemberRole::Member).unwrap();
        assert!(first.newly_joined);
        let again = s.join("room", "claude", MemberRole::Member).unwrap();
        assert!(!again.newly_joined);
        assert_eq!(s.members("room").unwrap().len(), 1);
    }

    #[tokio::test]
    async fn thirty_third_member_is_bounced() {
        let s = state();
        s.create_or_get_channel("crowded", "topic", "claude")
            .await
            .unwrap();
        for i in 0..MAX_MEMBERS {
            s.join("crowded", &format!("agent-{i}"), MemberRole::Member)
                .unwrap();
        }
        assert_eq!(s.members("crowded").unwrap().len(), MAX_MEMBERS);
        let err = s
            .join("crowded", "agent-32", MemberRole::Member)
            .unwrap_err();
        match err {
            BridgeError::ChannelFull {
                current,
                limit,
                next,
                ..
            } => {
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
        s.create_or_get_channel("chat", "topic", "claude")
            .await
            .unwrap();
        let m1 = s
            .post_message(
                "chat",
                "claude",
                Role::Assistant,
                "hello",
                serde_json::json!({}),
            )
            .unwrap();
        let m2 = s
            .post_message(
                "chat",
                "codex",
                Role::Assistant,
                "hi back",
                serde_json::json!({}),
            )
            .unwrap();
        assert_eq!(m1.seq, 1);
        assert_eq!(m2.seq, 2);
        // Both posters became members automatically.
        assert_eq!(s.members("chat").unwrap().len(), 2);
    }

    #[tokio::test]
    async fn resolve_reuses_close_topic_and_mints_distant_one() {
        let s = state();
        s.create_or_get_channel(
            "kubernetes-rollouts",
            "kubernetes deployment rollouts and argocd sync",
            "claude",
        )
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
        s.create_or_get_channel("ctx", "topic", "claude")
            .await
            .unwrap();
        let v1 = s
            .set_context("ctx", "plan", serde_json::json!({"step": 1}), "claude")
            .unwrap();
        assert_eq!(v1.version, 1);
        let v2 = s
            .set_context("ctx", "plan", serde_json::json!({"step": 2}), "codex")
            .unwrap();
        assert_eq!(v2.version, 2);
        assert_eq!(
            s.get_context_key("ctx", "plan").unwrap().unwrap().value,
            serde_json::json!({"step": 2})
        );
    }

    #[tokio::test]
    async fn subscribe_receives_posted_message() {
        let s = state();
        s.create_or_get_channel("live", "topic", "claude")
            .await
            .unwrap();
        // Join before subscribing so no presence event precedes the message.
        s.join("live", "claude", MemberRole::Member).unwrap();
        let (mut rx, _) = s.subscribe("live", None).unwrap();
        s.post_message(
            "live",
            "claude",
            Role::Assistant,
            "ping",
            serde_json::json!({}),
        )
        .unwrap();
        let ev = rx.recv().await.unwrap();
        match ev {
            Event::Message(m) => assert_eq!(m.content, "ping"),
            other => panic!("expected message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn leave_emits_presence_and_updates_count() {
        let s = state();
        s.create_or_get_channel("presence", "topic", "claude")
            .await
            .unwrap();
        let (mut rx, _) = s.subscribe("presence", None).unwrap();
        s.join("presence", "codex", MemberRole::Member).unwrap();
        // First event is the join presence.
        match rx.recv().await.unwrap() {
            Event::Presence {
                event,
                member_count,
                ..
            } => {
                assert_eq!(event, PresenceKind::Joined);
                assert_eq!(member_count, 1);
            }
            other => panic!("expected join presence, got {other:?}"),
        }
        s.leave("presence", "codex").unwrap();
        match rx.recv().await.unwrap() {
            Event::Presence {
                event,
                member_count,
                ..
            } => {
                assert_eq!(event, PresenceKind::Left);
                assert_eq!(member_count, 0);
            }
            other => panic!("expected leave presence, got {other:?}"),
        }
    }
}
