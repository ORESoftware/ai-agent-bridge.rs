//! End-to-end HTTP tests: REST round-trips, semantic routing, the 32-member cap,
//! SSE streaming, and shared context.

mod common;

use futures::StreamExt;
use serde_json::{json, Value};
use std::time::Duration;

async fn post(client: &reqwest::Client, url: String, body: Value) -> (reqwest::StatusCode, Value) {
    let resp = client.post(url).json(&body).send().await.unwrap();
    let status = resp.status();
    let json = resp.json::<Value>().await.unwrap_or(Value::Null);
    (status, json)
}

async fn get(client: &reqwest::Client, url: String) -> Value {
    client.get(url).send().await.unwrap().json::<Value>().await.unwrap()
}

#[tokio::test]
async fn full_rest_flow_register_create_search_post_context() {
    let base = common::spawn_http(common::state()).await;
    let c = reqwest::Client::new();

    // Register two agents.
    for (key, kind) in [("claude", "claude"), ("codex", "codex")] {
        let (st, body) = post(&c, format!("{base}/agents/register"), json!({ "agent_key": key, "kind": kind })).await;
        assert!(st.is_success());
        assert_eq!(body["agent"]["agent_key"], key);
    }

    // Two topically distinct channels.
    post(&c, format!("{base}/channels"), json!({ "slug": "sprint-planning", "topic": "sprint planning roadmap and backlog grooming", "created_by": "claude" })).await;
    post(&c, format!("{base}/channels"), json!({ "slug": "k8s-deploys", "topic": "kubernetes deployment rollouts and argocd sync", "created_by": "codex" })).await;

    // Semantic search routes to the right topic.
    let (_, search) = post(&c, format!("{base}/channels/search"), json!({ "query": "kubernetes rollout argocd", "limit": 5 })).await;
    assert_eq!(search["results"][0]["slug"], "k8s-deploys", "search should rank the deploy channel first: {search}");

    // Resolve reuses a close topic rather than minting a new one.
    let (_, resolved) = post(&c, format!("{base}/channels/resolve"), json!({ "query": "sprint backlog grooming session", "created_by": "claude", "threshold": 0.2 })).await;
    assert_eq!(resolved["created"], false);
    assert_eq!(resolved["channel"]["slug"], "sprint-planning");

    // Post + read back.
    let (st, posted) = post(&c, format!("{base}/channels/k8s-deploys/messages"), json!({ "from": "claude", "role": "assistant", "content": "rolling out v2 now" })).await;
    assert!(st.is_success());
    assert_eq!(posted["message"]["seq"], 1);
    let msgs = get(&c, format!("{base}/channels/k8s-deploys/messages")).await;
    assert_eq!(msgs["messages"][0]["content"], "rolling out v2 now");

    // Posting auto-joined claude.
    let members = get(&c, format!("{base}/channels/k8s-deploys/members")).await;
    assert_eq!(members["members"].as_array().unwrap().len(), 1);

    // Shared context round-trips and versions.
    post(&c, format!("{base}/channels/k8s-deploys/context"), json!({ "key": "status", "value": { "phase": "rollout" }, "updated_by": "codex" })).await;
    // PUT is the documented verb; ensure it works too.
    let put = c.put(format!("{base}/channels/k8s-deploys/context")).json(&json!({ "key": "status", "value": { "phase": "done" }, "updated_by": "codex" })).send().await.unwrap();
    assert!(put.status().is_success());
    let ctx = get(&c, format!("{base}/channels/k8s-deploys/context")).await;
    let entry = &ctx["context"][0];
    assert_eq!(entry["key"], "status");
    assert_eq!(entry["value"]["phase"], "done");
    assert_eq!(entry["version"], 2);
}

#[tokio::test]
async fn claude_inbox_backward_compat() {
    let base = common::spawn_http(common::state()).await;
    let c = reqwest::Client::new();

    // Legacy GET /health shape.
    let health = get(&c, format!("{base}/health")).await;
    assert_eq!(health["service"], "ai-agent-bridge");
    assert_eq!(health["inbox_messages"], 0);

    // Legacy POST /claude appends to inbox.jsonl and returns {queued,id,note}.
    let (st, body) = post(
        &c,
        format!("{base}/claude"),
        json!({ "prompt": "the plateau broke at gen 291", "from": "codex", "topic": "soccer plateau" }),
    )
    .await;
    assert!(st.is_success());
    assert_eq!(body["queued"], true);
    assert!(body["id"].as_u64().unwrap() > 0);

    // /health now reports the queued message.
    let health2 = get(&c, format!("{base}/health")).await;
    assert_eq!(health2["inbox_messages"], 1);

    // Superset: the same message is also live on the chat bus.
    let msgs = get(&c, format!("{base}/channels/soccer-plateau/messages")).await;
    assert_eq!(msgs["messages"][0]["content"], "the plateau broke at gen 291");
    assert_eq!(msgs["messages"][0]["from"], "codex");
}

#[tokio::test]
async fn thirty_third_member_gets_409_channel_full() {
    let base = common::spawn_http(common::state()).await;
    let c = reqwest::Client::new();
    post(&c, format!("{base}/channels"), json!({ "slug": "capped", "topic": "a small room", "created_by": "claude" })).await;

    for i in 0..32 {
        let (st, _) = post(&c, format!("{base}/channels/capped/join"), json!({ "agent_key": format!("agent-{i}") })).await;
        assert!(st.is_success(), "join {i} should succeed");
    }
    let (st, body) = post(&c, format!("{base}/channels/capped/join"), json!({ "agent_key": "agent-33" })).await;
    assert_eq!(st.as_u16(), 409, "33rd join must be 409");
    assert_eq!(body["error"], "channel_full");
    assert_eq!(body["limit"], 32);
    assert_eq!(body["current"], 32);

    let members = get(&c, format!("{base}/channels/capped/members")).await;
    assert_eq!(members["members"].as_array().unwrap().len(), 32, "roster stays at the cap");
}

#[tokio::test]
async fn sse_stream_delivers_presence_and_messages() {
    let base = common::spawn_http(common::state()).await;
    let c = reqwest::Client::new();
    post(&c, format!("{base}/channels"), json!({ "slug": "live", "topic": "live streaming room", "created_by": "claude" })).await;

    // Open the SSE stream as an observer.
    let resp = c.get(format!("{base}/channels/live/stream")).send().await.unwrap();
    assert!(resp.status().is_success());
    let mut stream = resp.bytes_stream();

    // Concurrently, a second agent joins and posts.
    let base2 = base.clone();
    tokio::spawn(async move {
        let c2 = reqwest::Client::new();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let _ = c2.post(format!("{base2}/channels/live/messages")).json(&json!({ "from": "codex", "content": "hello stream" })).send().await;
    });

    // Read the stream until we observe the message (bounded).
    let seen = tokio::time::timeout(Duration::from_secs(5), async {
        let mut buf = String::new();
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.unwrap();
            buf.push_str(&String::from_utf8_lossy(&bytes));
            if buf.contains("hello stream") {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(seen, "SSE stream should have delivered the posted message");
}
