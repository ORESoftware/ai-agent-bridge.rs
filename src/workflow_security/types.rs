const CONFIG_ENV: &str = "WORKFLOW_ADAPTER_AUTH_JSON";
const MAX_CREDENTIALS: usize = 1_000;
const MAX_TOKEN_BYTES: usize = 4_096;
const MAX_KEY_BYTES: usize = 120;
const MAX_SCOPES: usize = 32;
const RESERVED_CONTEXT_PREFIXES: &[&str] = &["workflow.", "internal."];

const KNOWN_SCOPES: &[&str] = &[
    "workflow:create",
    "workflow:read",
    "workflow:submit",
    "workflow:admit",
    "workflow:usage",
    "agent:register",
    "agent:read",
    "channel:join",
    "channel:post",
    "channel:read",
    "lease:operate",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatedAdapter {
    pub token_id: String,
    pub agent_key: String,
    pub scopes: BTreeSet<String>,
}

struct Credential {
    token_id: String,
    token: String,
    agent_key: String,
    scopes: BTreeSet<String>,
}

pub struct WorkflowSecurity {
    global_bearer: Option<String>,
    credentials: Vec<Credential>,
    max_body_bytes: usize,
}

#[derive(Deserialize)]
struct CredentialDocument {
    credentials: Vec<CredentialInput>,
}

#[derive(Deserialize)]
struct CredentialInput {
    token_id: String,
    token: String,
    agent_key: String,
    scopes: Vec<String>,
    #[serde(default = "default_true")]
    enabled: bool,
}

#[derive(Clone, Copy)]
struct AccessRule {
    scope: &'static str,
    identity_field: Option<&'static str>,
}

const fn default_true() -> bool {
    true
}
