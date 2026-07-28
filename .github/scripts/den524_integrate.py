from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text()
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one match, found {count}: {old!r}")
    file.write_text(text.replace(old, new, 1))
    print(f"updated {path}: {old.splitlines()[0].strip()}")


state = Path("src/state.rs")
state_text = state.read_text()
if "pub(crate) fn set_context_internal(" not in state_text:
    replace_once(
        "src/state.rs",
        "    pub fn set_context(\n",
        "    pub(crate) fn set_context_internal(\n",
    )
if "pub(crate) fn get_context_internal(" not in state.read_text():
    replace_once(
        "src/state.rs",
        "    pub fn get_context(&self, slug: &str) -> BridgeResult<Vec<ContextEntry>> {\n",
        "    pub(crate) fn get_context_internal(\n        &self,\n        slug: &str,\n    ) -> BridgeResult<Vec<ContextEntry>> {\n",
    )
if "pub(crate) fn get_context_key_internal(" not in state.read_text():
    replace_once(
        "src/state.rs",
        "    pub fn get_context_key(&self, slug: &str, key: &str) -> BridgeResult<Option<ContextEntry>> {\n",
        "    pub(crate) fn get_context_key_internal(\n        &self,\n        slug: &str,\n        key: &str,\n    ) -> BridgeResult<Option<ContextEntry>> {\n",
    )

for path in ["src/orchestration/part2.rs", "src/blind_competition/storage.rs"]:
    file = Path(path)
    text = file.read_text()
    updated = text.replace(".get_context_key(", ".get_context_key_internal(")
    updated = updated.replace(".get_context(", ".get_context_internal(")
    updated = updated.replace(".set_context(", ".set_context_internal(")
    if updated != text:
        file.write_text(updated)
        print(f"updated internal context calls in {path}")

main = Path("src/main.rs")
text = main.read_text()
if "let tcp_auth = workflow_auth.clone();" not in text:
    marker = """    let workflow_auth = workflow_security::WorkflowSecurity::from_env(
        config.api_auth_bearer.clone(),
        config.max_http_body_bytes,
    )?;
"""
    if marker not in text:
        raise RuntimeError("main workflow auth marker missing")
    text = text.replace(marker, marker + "    let tcp_auth = workflow_auth.clone();\n", 1)
if "tcp::serve(tcp_state, tcp_listener, tcp_auth).await" not in text:
    old_serve = "tcp::serve(tcp_state, tcp_listener).await"
    if old_serve not in text:
        raise RuntimeError("main tcp serve marker missing")
    text = text.replace(
        old_serve,
        "tcp::serve(tcp_state, tcp_listener, tcp_auth).await",
        1,
    )
main.write_text(text)
print("updated src/main.rs")

tcp = Path("src/tcp.rs")
text = tcp.read_text()
if "use crate::tcp_security::TcpPrincipal;" not in text:
    text = text.replace(
        "use crate::state::AppState;\n",
        "use crate::state::AppState;\nuse crate::tcp_security::TcpPrincipal;\nuse crate::workflow_security::WorkflowSecurity;\n",
        1,
    )
if "security: Arc<WorkflowSecurity>" not in text:
    text = text.replace(
        "pub async fn serve(state: Arc<AppState>, listener: TcpListener) -> anyhow::Result<()> {",
        "pub async fn serve(\n    state: Arc<AppState>,\n    listener: TcpListener,\n    security: Arc<WorkflowSecurity>,\n) -> anyhow::Result<()> {",
        1,
    )
    old_spawn = """        let state = state.clone();
        tokio::spawn(async move {
            let _permit = permit; // released when the connection ends
            if let Err(e) = handle_conn(state, socket).await {
"""
    new_spawn = """        let state = state.clone();
        let security = security.clone();
        tokio::spawn(async move {
            let _permit = permit; // released when the connection ends
            if let Err(e) = handle_conn(state, socket, security).await {
"""
    if old_spawn not in text:
        raise RuntimeError("tcp spawn marker missing")
    text = text.replace(old_spawn, new_spawn, 1)
    text = text.replace(
        "async fn handle_conn(state: Arc<AppState>, socket: TcpStream) -> anyhow::Result<()> {",
        "async fn handle_conn(\n    state: Arc<AppState>,\n    socket: TcpStream,\n    security: Arc<WorkflowSecurity>,\n) -> anyhow::Result<()> {",
        1,
    )
if "let mut principal = TcpPrincipal::initial(&security);" not in text:
    text = text.replace(
        "    let mut authed = state.config.api_auth_bearer.is_none();\n",
        "    let mut principal = TcpPrincipal::initial(&security);\n",
        1,
    )
if '"auth": principal.hello_json()' not in text:
    text = text.replace(
        '&json!({ "ok": true, "hello": "ai-agent-bridge", "needs_auth": !authed, "max_members": crate::config::MAX_MEMBERS }),',
        '&json!({\n            "ok": true,\n            "hello": "ai-agent-bridge",\n            "needs_auth": !principal.authenticated(),\n            "max_members": crate::config::MAX_MEMBERS,\n            "auth": principal.hello_json(),\n        }),',
        1,
    )
if "match principal.authenticate(&security, token)" not in text:
    old_auth = """        if let Req::Auth { token } = &req {
            authed = match state.config.api_auth_bearer.as_deref() {
                Some(expected) => {
                    crate::config::constant_time_eq(expected.as_bytes(), token.as_bytes())
                }
                None => true,
            };
            // Exactly one response frame per request: ok=false here already means
            // the token was rejected.
            let mut resp = json!({ "ok": authed, "op": "auth" });
            if !authed {
                resp["error"] = json!("unauthorized");
            }
            write_line(&writer, &resp).await?;
            continue;
        }
"""
    new_auth = """        if let Req::Auth { token } = &req {
            match principal.authenticate(&security, token) {
                Ok(next) => {
                    principal = next;
                    write_line(
                        &writer,
                        &json!({ "ok": true, "op": "auth", "auth": principal.hello_json() }),
                    )
                    .await?;
                }
                Err(error) => write_line(&writer, &error.payload()).await?,
            }
            continue;
        }
"""
    if old_auth not in text:
        raise RuntimeError("tcp auth block missing")
    text = text.replace(old_auth, new_auth, 1)
text = text.replace("if !authed {", "if !principal.authenticated() {")
text = text.replace("if authed {", "if principal.authenticated() {")
if "principal.authorize(&req)" not in text:
    marker = """        let response = dispatch(&state, &writer, req, &mut sub_tasks, &mut subscribed).await;
"""
    replacement = """        if let Err(error) = principal.authorize(&req) {
            write_line(&writer, &error.payload()).await?;
            continue;
        }

        let response = dispatch(&state, &writer, req, &mut sub_tasks, &mut subscribed).await;
"""
    if marker not in text:
        raise RuntimeError("tcp dispatch marker missing")
    text = text.replace(marker, replacement, 1)
if "pub(crate) enum Req {" not in text:
    text = text.replace("enum Req {", "pub(crate) enum Req {", 1)
tcp.write_text(text)
print("updated src/tcp.rs")
