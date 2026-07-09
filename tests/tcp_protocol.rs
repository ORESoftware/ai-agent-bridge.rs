//! End-to-end TCP (JSONL) tests: request/response round-trips, a multi-agent
//! streaming group chat, and the 32-member cap bounce over the socket.

mod common;

use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

/// A minimal JSONL client: one object per line, each `recv` reads one line.
struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Client {
    async fn connect(addr: std::net::SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).await.unwrap();
        let (r, w) = stream.into_split();
        let mut client = Self { reader: BufReader::new(r), writer: w };
        // Consume the server hello.
        let hello = client.recv().await;
        assert_eq!(hello["hello"], "ai-agent-bridge");
        client
    }

    async fn send(&mut self, v: Value) {
        let mut line = serde_json::to_vec(&v).unwrap();
        line.push(b'\n');
        self.writer.write_all(&line).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn recv(&mut self) -> Value {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(5), self.reader.read_line(&mut line))
            .await
            .expect("recv timed out")
            .unwrap();
        assert!(n > 0, "connection closed unexpectedly");
        serde_json::from_str(line.trim()).unwrap()
    }

    /// Read lines until one satisfies `pred`, or time out.
    async fn recv_until(&mut self, pred: impl Fn(&Value) -> bool) -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let v = self.recv().await;
                if pred(&v) {
                    return v;
                }
            }
        })
        .await
        .expect("did not observe the expected frame in time")
    }
}

#[tokio::test]
async fn request_response_round_trip() {
    let addr = common::spawn_tcp(common::state()).await;
    let mut a = Client::connect(addr).await;

    a.send(json!({ "op": "register", "agent_key": "claude", "kind": "claude" })).await;
    assert_eq!(a.recv().await["agent"]["agent_key"], "claude");

    a.send(json!({ "op": "create_channel", "slug": "ops", "topic": "cluster operations", "created_by": "claude" })).await;
    assert_eq!(a.recv().await["channel"]["slug"], "ops");

    a.send(json!({ "op": "post", "channel": "ops", "from": "claude", "content": "first" })).await;
    let posted = a.recv().await;
    assert_eq!(posted["ok"], true);
    assert_eq!(posted["message"]["seq"], 1);

    a.send(json!({ "op": "history", "channel": "ops" })).await;
    let hist = a.recv().await;
    assert_eq!(hist["messages"][0]["content"], "first");
}

#[tokio::test]
async fn multi_agent_streaming_group_chat() {
    let addr = common::spawn_tcp(common::state()).await;

    // Claude opens the room and subscribes to the live feed.
    let mut claude = Client::connect(addr).await;
    claude.send(json!({ "op": "create_channel", "slug": "war-room", "topic": "incident response", "created_by": "claude" })).await;
    claude.recv().await;
    claude.send(json!({ "op": "subscribe", "channel": "war-room", "agent_key": "claude" })).await;
    claude.recv_until(|v| v["subscribed"] == "war-room").await;

    // Three other agents each post to the room on their own connections.
    for (agent, text) in [("codex", "codex online"), ("scout", "scout online"), ("worker-7", "worker online")] {
        let mut c = Client::connect(addr).await;
        c.send(json!({ "op": "post", "channel": "war-room", "from": agent, "content": text })).await;
        assert_eq!(c.recv().await["ok"], true);
    }

    // Claude's stream should surface all three messages (fan-out) ...
    for text in ["codex online", "scout online", "worker online"] {
        let ev = claude
            .recv_until(|v| v["type"] == "message" && v["content"] == text)
            .await;
        assert_eq!(ev["type"], "message");
        assert_eq!(ev["content"], text);
    }

    // ... and the room now has four members (claude + 3 posters).
    claude.send(json!({ "op": "members", "channel": "war-room" })).await;
    let members = claude.recv_until(|v| v["members"].is_array()).await;
    assert_eq!(members["members"].as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn thirty_third_join_is_bounced_with_warning() {
    let addr = common::spawn_tcp(common::state()).await;
    let mut a = Client::connect(addr).await;
    a.send(json!({ "op": "create_channel", "slug": "capped", "topic": "small room", "created_by": "claude" })).await;
    a.recv().await;

    for i in 0..32 {
        a.send(json!({ "op": "join", "channel": "capped", "agent_key": format!("agent-{i}") })).await;
        assert_eq!(a.recv().await["ok"], true, "join {i} should succeed");
    }
    a.send(json!({ "op": "join", "channel": "capped", "agent_key": "agent-33" })).await;
    let bounced = a.recv().await;
    assert_eq!(bounced["ok"], false);
    assert_eq!(bounced["error"], "channel_full");
    assert_eq!(bounced["warning"], "channel_full");
    assert_eq!(bounced["limit"], 32);
}

#[tokio::test]
async fn resolve_forms_topics_over_tcp() {
    let addr = common::spawn_tcp(common::state()).await;
    let mut a = Client::connect(addr).await;

    // First resolve on an empty server mints a new topic.
    a.send(json!({ "op": "resolve", "channel": "", "query": "database migration strategy", "created_by": "claude" })).await;
    let first = a.recv().await;
    assert_eq!(first["created"], true);
    let slug = first["channel"]["slug"].as_str().unwrap().to_string();

    // A near-identical query resolves back to the same topic.
    a.send(json!({ "op": "resolve", "query": "strategy for database migrations", "created_by": "codex", "threshold": 0.2 })).await;
    let second = a.recv().await;
    assert_eq!(second["created"], false);
    assert_eq!(second["channel"]["slug"], slug);
}
