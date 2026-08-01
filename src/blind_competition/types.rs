const BLIND_PLAN_CONTEXT_KEY: &str = "workflow.blind.plan.v1";
const BLIND_SUBMISSION_CONTEXT_PREFIX: &str = "workflow.blind.submission.v1.";
const BLIND_REVEAL_CONTEXT_KEY: &str = "workflow.blind.reveal.v1";
const MAX_BLIND_WORKERS: usize = 8;
const MAX_TITLE_BYTES: usize = 1_024;
const MAX_AGENT_KEY_BYTES: usize = 120;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlindWorker {
    pub ordinal: usize,
    pub agent_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlindCompetitionPlan {
    pub version: u32,
    pub id: String,
    pub channel: String,
    pub title: String,
    pub prompt: String,
    pub created_by: String,
    pub created_at: String,
    pub workers: Vec<BlindWorker>,
    pub reviewer_agent_key: String,
    #[serde(default)]
    pub meta: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlindSubmission {
    pub workflow_id: String,
    pub assignment_ordinal: usize,
    pub agent_key: String,
    pub content: String,
    #[serde(default)]
    pub meta: serde_json::Value,
    pub submitted_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlindReveal {
    pub workflow_id: String,
    pub reviewer_agent_key: String,
    pub revealed_at: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BlindCompetitionStage {
    Collecting,
    ReadyToReveal,
    Revealed,
}

#[derive(Clone, Debug, Serialize)]
pub struct BlindCompetitionView {
    pub plan: BlindCompetitionPlan,
    pub stage: BlindCompetitionStage,
    pub submission_count: usize,
    pub hidden_submission_count: usize,
    pub revealed: bool,
    pub reviewer_can_reveal: bool,
    pub submissions: Vec<BlindSubmission>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reveal: Option<BlindReveal>,
}

#[derive(Debug, Deserialize)]
struct CreateBlindCompetitionReq {
    title: String,
    prompt: String,
    created_by: String,
    worker_agent_keys: Vec<String>,
    reviewer_agent_key: String,
    #[serde(default)]
    meta: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct SubmitBlindCompetitionReq {
    agent_key: String,
    content: String,
    #[serde(default)]
    meta: serde_json::Value,
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

type ApiResult = Result<Json<serde_json::Value>, ApiError>;
