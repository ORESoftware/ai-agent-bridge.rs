//! TCP JSONL scoped identity, operation capability, and namespace tests.

mod common;

use std::sync::Arc;

use ai_agent_bridge::tcp;
use ai_agent_bridge::workflow_security::WorkflowSecurity;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};

struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Client {
    async fn connect(address: std::net::SocketAddr) -> Self {
        let stream = TcpStream::connect(address).await.unwrap();
        let (read, write) = stream.into_split();
        let mut client = Self {
            reader: BufReader::new(read),
            writer: write,
        };
        let hello = client.read().await;
        assert_eq!(hello["ok"], true);
        client
    }

    async fn request(&mut self, request: Value) -> Value {
        let mut bytes = serde_json::to_vec(&request).unwrap();
        bytes.push(b'\n');
        self.writer.write_all(&bytes).await.unwrap();
        self.writer.flush().await.unwrap();
        self.read().await
    }

    async fn read(&mut self) -> Value {
        let mut line = String::new();
        self.reader.read_line(&mut line).await.unwrap();
        assert!(!line.is_empty());
        serde_json::from_str(line.trim()).unwrap()
    }
}

async fn spawn(security: Arc<WorkflowSecurity>) -> std::net::SocketAddr {
    let state = common::state();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = tcp::serve(state, listener, security).await;
    });
    address
}

fn scoped_security() -> Arc<WorkflowSecurity> {
    WorkflowSecurity::from_json(
        Some("operator-secret".into()),
        r#"{
          "credentials": [
            {
              "token_id": "codex-primary",
              "token": "codex-secret-primary",
              "agent_key": "codex",
              "scopes": [
                "agent:register",
                "channel:create",
                "channel:join",
                "channel:post",
                "channel:read",
                "context:read",
                "context:write"
              ]
            },
            {
              "token_id": "codex-rotation",
              "token": "codex-secret-rotation",
              "agent_key": "codex",
              "scopes": ["channel:read"]
            }
          ]
        }"#,
        1024 * 1024,
    )
    .unwrap()
}

#[tokio::test]
async fn scoped_connection_enforces_capabilities_identity_and_context_namespaces() {
    let address = spawn(scoped_security()).await;
    let mut client = Client::connect(address).await;

    let auth = client
        .request(json!({"op":"auth","token":"codex-secret-primary"}))
        .await;
    assert_eq!(auth["ok"], true);
    assert_eq!(auth["auth"]["principal"], "adapter");
    assert_eq!(auth["auth"]["agent_key"], "codex");
    let rendered_auth = auth.to_string();
    assert!(!rendered_auth.contains("codex-secret"));
    assert!(!rendered_auth.contains("codex-primary"));

    let register = client
        .request(json!({
            "op":"register",
            "agent_key":"codex",
            "display_name":"Codex",
            "kind":"codex"
        }))
        .await;
    assert_eq!(register["ok"], true);

    let spoofed_register = client
        .request(json!({
            "op":"register",
            "agent_key":"claude",
            "display_name":"Claude",
            "kind":"claude"
        }))
        .await;
    assert_eq!(spoofed_register["error"], "adapter_identity_mismatch");

    let channel = client
        .request(json!({
            "op":"create_channel",
            "slug":"tcp-security",
            "topic":"TCP security",
            "created_by":"codex"
        }))
        .await;
    assert_eq!(channel["ok"], true);

    let public_context = client
        .request(json!({
            "op":"set_context",
            "channel":"tcp-security",
            "key":"public.note",
            "value":{"visible":true},
            "updated_by":"codex"
        }))
        .await;
    assert_eq!(public_context["ok"], true);

    let reserved_context = client
        .request(json!({
            "op":"set_context",
            "channel":"tcp-security",
            "key":"workflow.plan.v1",
            "value":{"forged":true},
            "updated_by":"codex"
        }))
        .await;
    assert_eq!(reserved_context["error"], "bad_request");
    assert!(reserved_context["message"]
        .as_str()
        .unwrap_or_default()
        .contains("reserved_context_namespace"));

    let get_context = client
        .request(json!({"op":"get_context","channel":"tcp-security"}))
        .await;
    assert_eq!(get_context["ok"], true);
    assert_eq!(get_context["context"].as_array().unwrap().len(), 1);
    assert_eq!(get_context["context"][0]["key"], "public.note");

    let spoofed_post = client
        .request(json!({
            "op":"post",
            "channel":"tcp-security",
            "from":"claude",
            "content":"spoofed"
        }))
        .await;
    assert_eq!(spoofed_post["error"], "adapter_identity_mismatch");

    let switched = client
        .request(json!({"op":"auth","token":"codex-secret-rotation"}))
        .await;
    assert_eq!(switched["error"], "principal_switch_denied");

    let mut rotation = Client::connect(address).await;
    assert_eq!(
        rotation
            .request(json!({"op":"auth","token":"codex-secret-rotation"}))
            .await["ok"],
        true
    );
    assert_eq!(
        rotation.request(json!({"op":"list_channels"})).await["ok"],
        true
    );
    assert_eq!(
        rotation
            .request(json!({
                "op":"post",
                "channel":"tcp-security",
                "from":"codex",
                "content":"not allowed"
            }))
            .await["error"],
        "scope_denied"
    );
}

#[tokio::test]
async fn operator_and_no_auth_compatibility_modes_remain_explicit() {
    let address = spawn(scoped_security()).await;
    let mut operator = Client::connect(address).await;
    let auth = operator
        .request(json!({"op":"auth","token":"operator-secret"}))
        .await;
    assert_eq!(auth["auth"]["principal"], "operator");
    assert_eq!(
        operator
            .request(json!({
                "op":"register",
                "agent_key":"arbitrary-operator-agent",
                "kind":"other"
            }))
            .await["ok"],
        true
    );

    let open_security = WorkflowSecurity::from_json(None, r#"{"credentials":[]}"#, 1024).unwrap();
    let open_address = spawn(open_security).await;
    let mut open = Client::connect(open_address).await;
    assert_eq!(
        open.request(json!({"op":"list_channels"})).await["ok"],
        true
    );
}

#[tokio::test]
async fn unauthenticated_connections_allow_ping_only() {
    let address = spawn(scoped_security()).await;
    let mut client = Client::connect(address).await;
    assert_eq!(client.request(json!({"op":"ping"})).await["pong"], true);
    assert_eq!(
        client.request(json!({"op":"list_channels"})).await["error"],
        "unauthorized"
    );
    assert_eq!(
        client
            .request(json!({"op":"auth","token":"invalid-secret"}))
            .await["error"],
        "unauthorized"
    );
}
