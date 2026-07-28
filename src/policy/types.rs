pub const POLICY_VERSION: &str = "2026-07-27.v1";
const MAX_POLICY_BODY_BYTES: usize = 262_144;
const MAX_PROVIDER_CANDIDATES: usize = 32;
const MAX_CAPABILITIES: usize = 64;
const MAX_CAPABILITY_BYTES: usize = 120;
const MAX_AGENT_KEY_BYTES: usize = 120;
const MAX_MODEL_BYTES: usize = 200;
const ABS_MAX_PROVIDERS: u8 = 8;
const ABS_MAX_ROUNDS: u8 = 4;
const ABS_MAX_WALL_CLOCK_MS: u64 = 3_600_000;
const ABS_MAX_INPUT_TOKENS: u64 = 2_000_000;
const ABS_MAX_OUTPUT_TOKENS: u64 = 500_000;
const ABS_MAX_RETRIES: u8 = 5;
const ABS_MAX_CONCURRENCY: u8 = 8;
const ABS_MAX_COST_MICRO_USD: u64 = 200_000_000;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum TaskRisk {
    #[default]
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DataSensitivity {
    Public,
    #[default]
    Internal,
    Confidential,
    Restricted,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderRole {
    Worker,
    Reviewer,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DegradationBehavior {
    FailClosed,
    ReduceProviderCount,
    FallbackToSingleWithHumanApproval,
    QueueUntilRequiredProvidersAreAvailable,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProviderCandidate {
    pub agent_key: String,
    pub kind: AgentKind,
    pub model: String,
    #[serde(default = "default_true")]
    pub available: bool,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub trusted_for_restricted: bool,
    #[serde(default = "default_health_score_bps")]
    pub health_score_bps: u16,
    #[serde(default)]
    pub p95_latency_ms: u64,
    #[serde(default)]
    pub estimated_cost_micro_usd: u64,
    #[serde(default)]
    pub max_context_tokens: u64,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct RequestedBudget {
    pub max_providers: Option<u8>,
    pub max_rounds: Option<u8>,
    pub max_wall_clock_ms: Option<u64>,
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub max_retries: Option<u8>,
    pub max_concurrency: Option<u8>,
    pub max_cost_micro_usd: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PolicyRequest {
    #[serde(default)]
    pub task_risk: TaskRisk,
    #[serde(default)]
    pub data_sensitivity: DataSensitivity,
    #[serde(default)]
    pub requested_mode: Option<WorkflowMode>,
    #[serde(default)]
    pub required_capabilities: Vec<String>,
    #[serde(default)]
    pub requires_repository_write: bool,
    #[serde(default)]
    pub expected_duration_ms: u64,
    #[serde(default)]
    pub requested_budget: RequestedBudget,
    pub providers: Vec<ProviderCandidate>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct BudgetLimits {
    pub max_providers: u8,
    pub max_rounds: u8,
    pub max_wall_clock_ms: u64,
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
    pub max_retries: u8,
    pub max_concurrency: u8,
    pub max_cost_micro_usd: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct SelectedProvider {
    pub ordinal: usize,
    pub agent_key: String,
    pub kind: AgentKind,
    pub model: String,
    pub role: ProviderRole,
    pub estimated_cost_micro_usd: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct PolicyDecision {
    pub policy_version: &'static str,
    pub allowed: bool,
    pub mode: WorkflowMode,
    pub selected_providers: Vec<SelectedProvider>,
    pub budget: BudgetLimits,
    pub require_human_approval: bool,
    pub require_fiducia_lease: bool,
    pub degradation_behavior: DegradationBehavior,
    pub reasons: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub denial_reason: Option<String>,
}

#[derive(Clone, Copy)]
struct BudgetProfile {
    max_providers: u8,
    max_rounds: u8,
    max_wall_clock_ms: u64,
    max_input_tokens: u64,
    max_output_tokens: u64,
    max_retries: u8,
    max_concurrency: u8,
    max_cost_micro_usd: u64,
}
