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
    client
        .get(url)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()
}

async fn spawn_fake_control_plane() -> String {
    use axum::{
        extract::Query,
        http::{HeaderMap, StatusCode},
        routing::{get, post},
        Json, Router,
    };
    use std::collections::HashMap;

    async fn acquire(headers: HeaderMap, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
        if headers.get("x-internal-auth").and_then(|v| v.to_str().ok()) != Some("bridge-secret") {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error":"unauthorized"})),
            );
        }
        (
            StatusCode::CREATED,
            Json(json!({
                "result": {"output": {
                    "acquired": true,
                    "holder": body["agent_key"],
                    "keys": ["git-file/fiducia-ai-agent-bridge.rs/src/http.rs"],
                    "fencing_token": 41,
                    "lease_expires_ms": 999999
                }},
                "echo": body
            })),
        )
    }

    async fn lookup(
        headers: HeaderMap,
        Query(query): Query<HashMap<String, String>>,
    ) -> (StatusCode, Json<Value>) {
        if headers.get("x-internal-auth").and_then(|v| v.to_str().ok()) != Some("bridge-secret") {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error":"unauthorized"})),
            );
        }
        (
            StatusCode::OK,
            Json(json!({
                "key": format!("git-file/{}/{}", query["repository"], query["path"]),
                "lock": {
                    "holder": "codex",
                    "fencing_token": 41,
                    "lease_expires_ms": 999999,
                    "held_keys": ["git-file/fiducia-ai-agent-bridge.rs/src/http.rs"],
                    "wait_queue": []
                }
            })),
        )
    }

    async fn release(headers: HeaderMap, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
        if headers.get("x-internal-auth").and_then(|v| v.to_str().ok()) != Some("bridge-secret") {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error":"unauthorized"})),
            );
        }
        (
            StatusCode::OK,
            Json(json!({"result":{"output":{"released":true}}, "echo": body})),
        )
    }

    let app = Router::new()
        .route("/v1/file-leases", get(lookup))
        .route("/v1/file-leases/acquire", post(acquire))
        .route("/v1/file-leases/release", post(release));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn file_leases_proxy_to_control_plane_and_enrich_the_active_agent() {
    let control_plane = spawn_fake_control_plane().await;
    let mut config = common::base_config();
    config.control_plane_url = Some(control_plane);
    config.control_plane_secret = Some("bridge-secret".into());
    let base = common::spawn_http(common::state_with(config)).await;
    let client = reqwest::Client::new();

    post(
        &client,
        format!("{base}/agents/register"),
        json!({"agent_key":"codex", "display_name":"Codex worker", "kind":"codex"}),
    )
    .await;
    let (status, acquired) = post(
        &client,
        format!("{base}/file-leases/acquire"),
        json!({
            "repository":"fiducia-ai-agent-bridge.rs",
            "paths":["src/http.rs"],
            "agent_key":"codex",
            "ttl_ms":45000
        }),
    )
    .await;
    assert_eq!(status.as_u16(), 201);
    assert_eq!(acquired["result"]["output"]["fencing_token"], 41);
    assert_eq!(acquired["echo"]["ttl_ms"], 45000);

    let lookup = get(
        &client,
        format!("{base}/file-leases?repository=fiducia-ai-agent-bridge.rs&path=src%2Fhttp.rs"),
    )
    .await;
    assert_eq!(lookup["lock"]["holder"], "codex");
    assert_eq!(lookup["agent"]["display_name"], "Codex worker");

    let (status, released) = post(
        &client,
        format!("{base}/file-leases/release"),
        json!({"agent_key":"codex", "fencing_token":41}),
    )
    .await;
    assert!(status.is_success());
    assert_eq!(released["echo"]["agent_key"], "codex");
}

#[tokio::test]
async fn full_rest_flow_register_create_search_post_context() {
    let base = common::spawn_http(common::state()).await;
    let c = reqwest::Client::new();

    // Register two agents.
    for (key, kind) in [("claude", "claude"), ("codex", "codex")] {
        let (st, body) = post(
            &c,
            format!("{base}/agents/register"),
            json!({ "agent_key": key, "kind": kind }),
        )
        .await;
        assert!(st.is_success());
        assert_eq!(body["agent"]["agent_key"], key);
    }

    // Two topically distinct channels.
    post(&c, format!("{base}/channels"), json!({ "slug": "sprint-planning", "topic": "sprint planning roadmap and backlog grooming", "created_by": "claude" })).await;
    post(&c, format!("{base}/channels"), json!({ "slug": "k8s-deploys", "topic": "kubernetes deployment rollouts and argocd sync", "created_by": "codex" })).await;

    // Semantic search routes to the right topic.
    let (_, search) = post(
        &c,
        format!("{base}/channels/search"),
        json!({ "query": "kubernetes rollout argocd", "limit": 5 }),
    )
    .await;
    assert_eq!(
        search["results"][0]["slug"], "k8s-deploys",
        "search should rank the deploy channel first: {search}"
    );

    // Resolve reuses a close topic rather than minting a new one.
    let (_, resolved) = post(&c, format!("{base}/channels/resolve"), json!({ "query": "sprint backlog grooming session", "created_by": "claude", "threshold": 0.2 })).await;
    assert_eq!(resolved["created"], false);
    assert_eq!(resolved["channel"]["slug"], "sprint-planning");

    // Post + read back.
    let (st, posted) = post(
        &c,
        format!("{base}/channels/k8s-deploys/messages"),
        json!({ "from": "claude", "role": "assistant", "content": "rolling out v2 now" }),
    )
    .await;
    assert!(st.is_success());
    assert_eq!(posted["message"]["seq"], 1);
    let msgs = get(&c, format!("{base}/channels/k8s-deploys/messages")).await;
    assert_eq!(msgs["messages"][0]["content"], "rolling out v2 now");

    // Posting auto-joined claude.
    let members = get(&c, format!("{base}/channels/k8s-deploys/members")).await;
    assert_eq!(members["members"].as_array().unwrap().len(), 1);

    // Shared context round-trips and versions.
    post(
        &c,
        format!("{base}/channels/k8s-deploys/context"),
        json!({ "key": "status", "value": { "phase": "rollout" }, "updated_by": "codex" }),
    )
    .await;
    // PUT is the documented verb; ensure it works too.
    let put = c
        .put(format!("{base}/channels/k8s-deploys/context"))
        .json(&json!({ "key": "status", "value": { "phase": "done" }, "updated_by": "codex" }))
        .send()
        .await
        .unwrap();
    assert!(put.status().is_success());
    let ctx = get(&c, format!("{base}/channels/k8s-deploys/context")).await;
    let entry = &ctx["context"][0];
    assert_eq!(entry["key"], "status");
    assert_eq!(entry["value"]["phase"], "done");
    assert_eq!(entry["version"], 2);
}

#[tokio::test]
async fn file_leases_fence_writers_and_join_agents_to_paths() {
    let base = common::spawn_http(common::state()).await;
    let c = reqwest::Client::new();
    for key in ["codex", "claude"] {
        let (status, _) = post(
            &c,
            format!("{base}/agents/register"),
            json!({ "agent_key": key, "kind": key }),
        )
        .await;
        assert!(status.is_success());
    }

    let (status, acquired) = post(
        &c,
        format!("{base}/file-leases"),
        json!({
            "repository": "fiducia-cloud/fiducia-ai-agent-bridge.rs",
            "path": "src",
            "recursive": true,
            "agent_key": "codex",
            "purpose": "implement file ownership API",
            "ttl_ms": 30_000
        }),
    )
    .await;
    assert_eq!(status.as_u16(), 200);
    let lease_id = acquired["lease"]["id"].as_str().unwrap();
    let token = acquired["lease"]["fencing_token"].as_u64().unwrap();

    // A recursive directory lease blocks a different agent on any child file.
    let (status, conflict) = post(
        &c,
        format!("{base}/file-leases"),
        json!({
            "repository": "fiducia-cloud/fiducia-ai-agent-bridge.rs",
            "path": "src/http.rs",
            "agent_key": "claude",
            "ttl_ms": 30_000
        }),
    )
    .await;
    assert_eq!(status.as_u16(), 409);
    assert_eq!(conflict["error"], "file_lease_conflict");

    // File lookup returns the active lease and the full registered agent record.
    let lookup = c
        .get(format!("{base}/agents/by-file"))
        .query(&[
            ("repository", "fiducia-cloud/fiducia-ai-agent-bridge.rs"),
            ("path", "src/http.rs"),
        ])
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(lookup["assignments"][0]["agent"]["agent_key"], "codex");
    assert_eq!(lookup["assignments"][0]["lease"]["path"], "src");

    let (status, stale) = post(
        &c,
        format!("{base}/file-leases/{lease_id}/renew"),
        json!({ "agent_key": "codex", "fencing_token": token + 1, "ttl_ms": 30_000 }),
    )
    .await;
    assert_eq!(status.as_u16(), 409);
    assert_eq!(stale["error"], "stale_fencing_token");

    let (status, _) = post(
        &c,
        format!("{base}/file-leases/{lease_id}/release"),
        json!({ "agent_key": "codex", "fencing_token": token }),
    )
    .await;
    assert!(status.is_success());

    let (status, successor) = post(
        &c,
        format!("{base}/file-leases"),
        json!({
            "repository": "fiducia-cloud/fiducia-ai-agent-bridge.rs",
            "path": "src/http.rs",
            "agent_key": "claude",
            "ttl_ms": 100
        }),
    )
    .await;
    assert!(status.is_success());
    assert!(successor["lease"]["fencing_token"].as_u64().unwrap() > token);

    // Expired leases disappear from file ownership lookups.
    tokio::time::sleep(Duration::from_millis(125)).await;
    let lookup = c
        .get(format!("{base}/agents/by-file"))
        .query(&[
            ("repository", "fiducia-cloud/fiducia-ai-agent-bridge.rs"),
            ("path", "src/http.rs"),
        ])
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(lookup["assignments"].as_array().unwrap().len(), 0);
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
    assert_eq!(
        msgs["messages"][0]["content"],
        "the plateau broke at gen 291"
    );
    assert_eq!(msgs["messages"][0]["from"], "codex");
}

#[tokio::test]
async fn thirty_third_member_gets_409_channel_full() {
    let base = common::spawn_http(common::state()).await;
    let c = reqwest::Client::new();
    post(
        &c,
        format!("{base}/channels"),
        json!({ "slug": "capped", "topic": "a small room", "created_by": "claude" }),
    )
    .await;

    for i in 0..32 {
        let (st, _) = post(
            &c,
            format!("{base}/channels/capped/join"),
            json!({ "agent_key": format!("agent-{i}") }),
        )
        .await;
        assert!(st.is_success(), "join {i} should succeed");
    }
    let (st, body) = post(
        &c,
        format!("{base}/channels/capped/join"),
        json!({ "agent_key": "agent-33" }),
    )
    .await;
    assert_eq!(st.as_u16(), 409, "33rd join must be 409");
    assert_eq!(body["error"], "channel_full");
    assert_eq!(body["limit"], 32);
    assert_eq!(body["current"], 32);

    let members = get(&c, format!("{base}/channels/capped/members")).await;
    assert_eq!(
        members["members"].as_array().unwrap().len(),
        32,
        "roster stays at the cap"
    );
}

/// The 32-member cap counts LIVE membership, not historical joins: a leave
/// frees exactly one slot for a new agent, and the room is full again once
/// that slot is taken.
#[tokio::test]
async fn leave_frees_a_slot_at_the_member_cap() {
    let base = common::spawn_http(common::state()).await;
    let c = reqwest::Client::new();
    post(
        &c,
        format!("{base}/channels"),
        json!({ "slug": "revolving", "topic": "a full room", "created_by": "claude" }),
    )
    .await;

    for i in 0..32 {
        let (st, _) = post(
            &c,
            format!("{base}/channels/revolving/join"),
            json!({ "agent_key": format!("agent-{i}") }),
        )
        .await;
        assert!(st.is_success(), "join {i} should succeed");
    }
    let (st, body) = post(
        &c,
        format!("{base}/channels/revolving/join"),
        json!({ "agent_key": "latecomer" }),
    )
    .await;
    assert_eq!(st.as_u16(), 409, "the room starts full");
    assert_eq!(body["error"], "channel_full");

    // One member leaves; the freed slot admits the latecomer.
    let (st, body) = post(
        &c,
        format!("{base}/channels/revolving/leave"),
        json!({ "agent_key": "agent-0" }),
    )
    .await;
    assert!(st.is_success());
    assert_eq!(body["removed"], true);

    let (st, body) = post(
        &c,
        format!("{base}/channels/revolving/join"),
        json!({ "agent_key": "latecomer" }),
    )
    .await;
    assert!(st.is_success(), "leave must free a slot: {body}");
    assert_eq!(body["newly_joined"], true);

    // The roster is back at the cap with the leaver replaced by the newcomer.
    let members = get(&c, format!("{base}/channels/revolving/members")).await;
    let keys: Vec<&str> = members["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["agent_key"].as_str().unwrap())
        .collect();
    assert_eq!(keys.len(), 32, "roster returns to the cap");
    assert!(keys.contains(&"latecomer"));
    assert!(!keys.contains(&"agent-0"), "the leaver stays gone");

    // And the room is full again: the next join is refused.
    let (st, body) = post(
        &c,
        format!("{base}/channels/revolving/join"),
        json!({ "agent_key": "too-late" }),
    )
    .await;
    assert_eq!(st.as_u16(), 409, "the reclaimed slot re-fills the room");
    assert_eq!(body["error"], "channel_full");
}

#[tokio::test]
async fn sse_stream_delivers_presence_and_messages() {
    let base = common::spawn_http(common::state()).await;
    let c = reqwest::Client::new();
    post(
        &c,
        format!("{base}/channels"),
        json!({ "slug": "live", "topic": "live streaming room", "created_by": "claude" }),
    )
    .await;

    // Open the SSE stream as an observer.
    let resp = c
        .get(format!("{base}/channels/live/stream"))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let mut stream = resp.bytes_stream();

    // Concurrently, a second agent joins and posts.
    let base2 = base.clone();
    tokio::spawn(async move {
        let c2 = reqwest::Client::new();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let _ = c2
            .post(format!("{base2}/channels/live/messages"))
            .json(&json!({ "from": "codex", "content": "hello stream" }))
            .send()
            .await;
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
