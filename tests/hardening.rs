//! Security + resource-limit hardening tests over the real transports.

mod common;

use serde_json::{json, Value};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

// ---- HTTP -------------------------------------------------------------------

#[tokio::test]
async fn bearer_auth_gates_the_api_but_not_health() {
    let mut cfg = common::base_config();
    cfg.api_auth_bearer = Some("s3cret".into());
    let base = common::spawn_http(common::state_with(cfg)).await;
    let c = reqwest::Client::new();

    // Health is exempt.
    assert!(c
        .get(format!("{base}/healthz"))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());

    // API requires the bearer.
    assert_eq!(
        c.get(format!("{base}/channels"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        401
    );
    assert_eq!(
        c.get(format!("{base}/channels"))
            .bearer_auth("wrong")
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        401
    );
    assert!(c
        .get(format!("{base}/channels"))
        .bearer_auth("s3cret")
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
}

#[tokio::test]
async fn claude_route_respects_global_bearer_when_no_inbox_token() {
    // Regression: /claude used to bypass API_AUTH_BEARER when inbox_token was unset.
    let mut cfg = common::base_config();
    cfg.api_auth_bearer = Some("global".into());
    cfg.inbox_token = None;
    let base = common::spawn_http(common::state_with(cfg)).await;
    let c = reqwest::Client::new();

    let un = c
        .post(format!("{base}/claude"))
        .json(&json!({ "prompt": "hi", "from": "codex" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        un.status().as_u16(),
        401,
        "/claude must not bypass the global bearer"
    );

    let ok = c
        .post(format!("{base}/claude"))
        .bearer_auth("global")
        .json(&json!({ "prompt": "hi", "from": "codex" }))
        .send()
        .await
        .unwrap();
    assert!(ok.status().is_success());
}

#[tokio::test]
async fn oversized_message_returns_413() {
    let mut cfg = common::base_config();
    cfg.max_content_bytes = 64;
    let base = common::spawn_http(common::state_with(cfg)).await;
    let c = reqwest::Client::new();
    c.post(format!("{base}/channels"))
        .json(&json!({ "slug": "room", "topic": "t", "created_by": "claude" }))
        .send()
        .await
        .unwrap();
    let resp = c
        .post(format!("{base}/channels/room/messages"))
        .json(&json!({ "from": "claude", "content": "z".repeat(100) }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 413);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "payload_too_large");
    assert_eq!(body["limit"], 64);
}

#[tokio::test]
async fn unknown_channel_is_404_and_bad_json_is_generic() {
    let base = common::spawn_http(common::state()).await;
    let c = reqwest::Client::new();

    assert_eq!(
        c.get(format!("{base}/channels/nope/members"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        404
    );

    // /claude with malformed JSON returns a generic 400 (no parser offsets leaked).
    let resp = c
        .post(format!("{base}/claude"))
        .header("content-type", "application/json")
        .body("{ not json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "invalid json body");
}

// ---- TCP --------------------------------------------------------------------

async fn tcp_line(
    addr: std::net::SocketAddr,
) -> (
    BufReader<tokio::net::tcp::OwnedReadHalf>,
    tokio::net::tcp::OwnedWriteHalf,
) {
    let (r, w) = TcpStream::connect(addr).await.unwrap().into_split();
    let mut reader = BufReader::new(r);
    let mut hello = String::new();
    reader.read_line(&mut hello).await.unwrap(); // consume hello
    (reader, w)
}

#[tokio::test]
async fn tcp_oversized_frame_drops_the_connection() {
    let mut cfg = common::base_config();
    cfg.max_tcp_line_bytes = 1024;
    let addr = common::spawn_tcp(common::state_with(cfg)).await;
    let (mut reader, mut w) = tcp_line(addr).await;

    // A 4 KiB frame with no newline exceeds the 1 KiB cap; the server must drop
    // the connection rather than buffer without bound.
    let mut junk = vec![b'x'; 4096];
    junk.push(b'\n');
    let _ = w.write_all(&junk).await;
    let _ = w.flush().await;

    // Reading should hit EOF (connection closed) quickly, not hang or OOM.
    let mut line = String::new();
    let n = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("server should have closed the connection")
        .unwrap();
    assert_eq!(n, 0, "expected EOF after an oversized frame");
}

async fn send(w: &mut tokio::net::tcp::OwnedWriteHalf, v: Value) {
    let mut line = serde_json::to_vec(&v).unwrap();
    line.push(b'\n');
    w.write_all(&line).await.unwrap();
    w.flush().await.unwrap();
}

async fn recv(reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> Value {
    let mut l = String::new();
    reader.read_line(&mut l).await.unwrap();
    serde_json::from_str(l.trim()).unwrap()
}

#[tokio::test]
async fn tcp_duplicate_subscribe_to_same_channel_rejected() {
    let addr = common::spawn_tcp(common::state()).await;
    let (r, mut w) = TcpStream::connect(addr).await.unwrap().into_split();
    let mut reader = BufReader::new(r);
    recv(&mut reader).await; // hello

    send(
        &mut w,
        json!({ "op": "create_channel", "slug": "room", "topic": "t" }),
    )
    .await;
    recv(&mut reader).await;

    send(&mut w, json!({ "op": "subscribe", "channel": "room" })).await;
    // No history, so the first frame back is the subscribed ack.
    assert_eq!(recv(&mut reader).await["subscribed"], "room");

    // A second subscribe to the same channel on this connection is rejected
    // (would otherwise duplicate every live event).
    send(&mut w, json!({ "op": "subscribe", "channel": "room" })).await;
    assert_eq!(recv(&mut reader).await["error"], "already_subscribed");
}

#[tokio::test]
async fn claude_inbox_line_key_order_is_stable() {
    let mut cfg = common::base_config();
    let dir = cfg.inbox_dir.clone();
    cfg.inbox_dir = dir.clone();
    let base = common::spawn_http(common::state_with(cfg)).await;
    let c = reqwest::Client::new();
    c.post(format!("{base}/claude"))
        .json(&json!({ "prompt": "hello", "from": "codex", "topic": "t" }))
        .send()
        .await
        .unwrap();

    let contents = std::fs::read_to_string(dir.join("inbox.jsonl")).unwrap();
    let line = contents.lines().next().unwrap();
    // Typed struct -> exact order id, ts, from, topic, prompt (a Value would sort).
    let pos = |k: &str| line.find(k).unwrap_or(usize::MAX);
    assert!(
        line.starts_with(r#"{"id":"#),
        "unexpected inbox line: {line}"
    );
    assert!(pos("\"id\"") < pos("\"ts\""));
    assert!(pos("\"ts\"") < pos("\"from\""));
    assert!(pos("\"from\"") < pos("\"topic\""));
    assert!(pos("\"topic\"") < pos("\"prompt\""));
}

#[tokio::test]
async fn tcp_unauthenticated_connection_dropped_on_deadline() {
    let mut cfg = common::base_config();
    cfg.api_auth_bearer = Some("tok".into());
    cfg.tcp_auth_deadline_secs = 1; // fast for the test
    let addr = common::spawn_tcp(common::state_with(cfg)).await;
    let (r, _w) = TcpStream::connect(addr).await.unwrap().into_split();
    let mut reader = BufReader::new(r);
    recv(&mut reader).await; // hello

    // Send nothing. Within ~1s the server should warn and close the connection,
    // so an idle unauthenticated socket can't squat a slot.
    let frame = tokio::time::timeout(Duration::from_secs(5), recv(&mut reader))
        .await
        .expect("expected an auth_timeout frame");
    assert_eq!(frame["error"], "auth_timeout");
    let mut line = String::new();
    let n = tokio::time::timeout(Duration::from_secs(3), reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(n, 0, "connection should be closed after the auth deadline");
}

#[tokio::test]
async fn tcp_auth_handshake_enforced() {
    let mut cfg = common::base_config();
    cfg.api_auth_bearer = Some("tok".into());
    let addr = common::spawn_tcp(common::state_with(cfg)).await;
    let (r, mut w) = TcpStream::connect(addr).await.unwrap().into_split();
    let mut reader = BufReader::new(r);

    let hello = recv(&mut reader).await;
    assert_eq!(hello["needs_auth"], true);

    // An op before auth is rejected.
    send(&mut w, json!({ "op": "list_channels" })).await;
    assert_eq!(recv(&mut reader).await["error"], "unauthorized");

    // Wrong token rejected.
    send(&mut w, json!({ "op": "auth", "token": "nope" })).await;
    assert_eq!(recv(&mut reader).await["ok"], false);

    // Correct token accepted, then ops work.
    send(&mut w, json!({ "op": "auth", "token": "tok" })).await;
    assert_eq!(recv(&mut reader).await["ok"], true);
    send(&mut w, json!({ "op": "list_channels" })).await;
    assert_eq!(recv(&mut reader).await["ok"], true);
}
