const ADMISSION_CONTEXT_KEY: &str = "workflow.admission.v1";
const WORKFLOW_PLAN_CONTEXT_KEY: &str = "workflow.plan.v1";
const MAX_ACTOR_BYTES: usize = 120;
const MAX_REASON_BYTES: usize = 2_048;
const MAX_ADMISSION_BODY_BYTES: usize = 512 * 1024;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionStatus {
    Active,
    Exhausted,
    Completed,
    Cancelled,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_micro_usd: u64,
    pub retries: u64,
    pub provider_calls: u64,
    pub elapsed_ms: u64,
    pub peak_concurrency: u8,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageDelta {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cost_micro_usd: u64,
    #[serde(default)]
    pub retries: u64,
    #[serde(default)]
    pub provider_calls: u64,
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(default)]
    pub concurrency: u8,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdmissionPolicySnapshot {
    pub policy_version: String,
    pub mode: WorkflowMode,
    pub selected_agent_keys: Vec<String>,
    pub budget: BudgetLimits,
    pub require_human_approval: bool,
    pub require_fiducia_lease: bool,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdmissionRecord {
    pub version: u32,
    pub workflow_id: String,
    pub requested_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_reason: Option<String>,
    pub policy: AdmissionPolicySnapshot,
    pub status: AdmissionStatus,
    pub usage: UsageTotals,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_rejected_delta: Option<UsageDelta>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AdmissionEnvelope {
    pub ok: bool,
    pub created: bool,
    pub admission: AdmissionRecord,
}

#[derive(Debug, Deserialize)]
struct AdmitReq {
    requested_by: String,
    #[serde(default)]
    approved_by: Option<String>,
    #[serde(default)]
    override_reason: Option<String>,
    policy_request: PolicyRequest,
}

#[derive(Debug, Deserialize)]
struct UsageReq {
    updated_by: String,
    delta: UsageDelta,
}

#[derive(Debug, Deserialize)]
struct TerminalReq {
    updated_by: String,
    #[serde(default)]
    reason: Option<String>,
}

struct ApiError(BridgeError);

impl From<BridgeError> for ApiError {
    fn from(error: BridgeError) -> Self {
        Self(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(self.0.payload())).into_response()
    }
}

type ApiResult = Result<Json<Value>, ApiError>;
