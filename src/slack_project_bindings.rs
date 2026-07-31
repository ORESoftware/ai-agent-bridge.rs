use std::collections::{BTreeMap, BTreeSet};

use chrono::DateTime;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const SCHEMA_VERSION: u32 = 1;
const MAX_BINDINGS: usize = 1_000;
const MAX_IDENTIFIER_BYTES: usize = 255;
const MAX_REPOSITORIES_PER_BINDING: usize = 100;
const MAX_PRINCIPALS_PER_BINDING: usize = 1_000;
const MAX_CONCURRENT_RUNS: u16 = 128;
const MAX_RUNTIME_SECS: u32 = 3_600;
const MAX_TOKENS: u64 = 10_000_000;
const MAX_SPEND_CENTS: u64 = 100_000;
const MAX_RETRIES: u8 = 10;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentMode {
    Claude,
    Chatgpt,
    BothParallel,
    BothSequential,
    Review,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WritePolicy {
    ReadOnly,
    LinearOnly,
    DraftPullRequest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestedCapability {
    ReadOnly,
    LinearWrite,
    RepositoryWrite,
}

impl WritePolicy {
    fn permits(self, requested: RequestedCapability) -> bool {
        match (self, requested) {
            (_, RequestedCapability::ReadOnly)
            | (Self::LinearOnly | Self::DraftPullRequest, RequestedCapability::LinearWrite)
            | (Self::DraftPullRequest, RequestedCapability::RepositoryWrite) => true,
            (
                Self::ReadOnly,
                RequestedCapability::LinearWrite | RequestedCapability::RepositoryWrite,
            )
            | (Self::LinearOnly, RequestedCapability::RepositoryWrite) => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetPolicy {
    pub max_concurrent_runs: u16,
    pub max_runtime_secs: u32,
    pub max_tokens: u64,
    pub max_spend_cents: u64,
    pub max_retries: u8,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelProjectBinding {
    pub workspace_id: String,
    pub channel_id: String,
    pub linear_team_id: String,
    pub linear_team_key: String,
    pub linear_project_id: String,
    pub default_repository: String,
    pub repository_allowlist: BTreeSet<String>,
    pub default_agent_mode: AgentMode,
    pub allowed_agent_modes: BTreeSet<AgentMode>,
    pub allowed_user_ids: BTreeSet<String>,
    pub allowed_user_group_ids: BTreeSet<String>,
    pub write_policy: WritePolicy,
    pub budget_policy: BudgetPolicy,
    pub updated_by: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SlackProjectRegistryDocument {
    pub schema_version: u32,
    pub bindings: Vec<ChannelProjectBinding>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolveRequest {
    pub workspace_id: String,
    pub channel_id: String,
    pub user_id: String,
    pub user_group_ids: BTreeSet<String>,
    pub requested_repository: Option<String>,
    pub requested_agent_mode: Option<AgentMode>,
    pub requested_capability: RequestedCapability,
    pub linear_issue_identifier: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinearIssueRef {
    pub identifier: String,
    pub team_key: String,
    pub number: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedProjectContext {
    pub workspace_id: String,
    pub channel_id: String,
    pub linear_team_id: String,
    pub linear_team_key: String,
    pub linear_project_id: String,
    pub repository: String,
    pub agent_mode: AgentMode,
    pub write_policy: WritePolicy,
    pub budget_policy: BudgetPolicy,
    pub issue: Option<LinearIssueRef>,
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("invalid Slack project registry JSON")]
    Json(#[from] serde_json::Error),
    #[error("unsupported Slack project registry schema version")]
    UnsupportedSchema,
    #[error("Slack project registry contains too many bindings")]
    TooManyBindings,
    #[error("Slack project registry contains an invalid stable identifier")]
    InvalidIdentifier,
    #[error("Slack project registry contains an invalid Linear team key")]
    InvalidTeamKey,
    #[error("Slack project registry contains an invalid repository")]
    InvalidRepository,
    #[error("Slack project registry contains too many repositories")]
    TooManyRepositories,
    #[error("Slack project registry contains too many authorized principals")]
    TooManyPrincipals,
    #[error("Slack project registry binding has no authorized principal")]
    MissingPrincipal,
    #[error("Slack project registry binding has no allowed agent mode")]
    MissingAgentMode,
    #[error("default repository is absent from the repository allowlist")]
    DefaultRepositoryNotAllowed,
    #[error("default agent mode is absent from the agent-mode allowlist")]
    DefaultAgentModeNotAllowed,
    #[error("Slack project registry contains an invalid budget")]
    InvalidBudget,
    #[error("Slack project registry contains an invalid audit timestamp")]
    InvalidAuditTimestamp,
    #[error("duplicate workspace and channel binding")]
    DuplicateBinding,
    #[error("Slack channel is not mapped to a Linear project")]
    UnmappedChannel,
    #[error("Slack user is not authorized for the mapped project")]
    UnauthorizedPrincipal,
    #[error("requested repository is not allowed for the mapped project")]
    RepositoryNotAllowed,
    #[error("requested agent mode is not allowed for the mapped project")]
    AgentModeNotAllowed,
    #[error("requested write capability is not allowed for the mapped project")]
    WriteNotAllowed,
    #[error("invalid Linear issue identifier")]
    InvalidIssueIdentifier,
    #[error("Linear issue identifier belongs to a different team")]
    IssueTeamMismatch,
}

#[derive(Clone, Debug)]
pub struct SlackProjectRegistry {
    bindings: BTreeMap<(String, String), ChannelProjectBinding>,
}

impl SlackProjectRegistry {
    pub fn from_json(input: &[u8]) -> Result<Self, RegistryError> {
        let mut document = serde_json::from_slice::<SlackProjectRegistryDocument>(input)?;
        if document.schema_version != SCHEMA_VERSION {
            return Err(RegistryError::UnsupportedSchema);
        }
        if document.bindings.len() > MAX_BINDINGS {
            return Err(RegistryError::TooManyBindings);
        }

        let mut bindings = BTreeMap::new();
        for binding in &mut document.bindings {
            validate_binding(binding)?;
            let key = (binding.workspace_id.clone(), binding.channel_id.clone());
            if bindings.insert(key, binding.clone()).is_some() {
                return Err(RegistryError::DuplicateBinding);
            }
        }
        Ok(Self { bindings })
    }

    pub fn resolve(
        &self,
        request: &ResolveRequest,
    ) -> Result<ResolvedProjectContext, RegistryError> {
        validate_stable_identifier(&request.workspace_id)?;
        validate_stable_identifier(&request.channel_id)?;
        validate_stable_identifier(&request.user_id)?;
        for group_id in &request.user_group_ids {
            validate_stable_identifier(group_id)?;
        }

        let key = (request.workspace_id.clone(), request.channel_id.clone());
        let binding = self
            .bindings
            .get(&key)
            .ok_or(RegistryError::UnmappedChannel)?;

        let authorized_by_user = binding.allowed_user_ids.contains(&request.user_id);
        let authorized_by_group = request
            .user_group_ids
            .iter()
            .any(|group_id| binding.allowed_user_group_ids.contains(group_id));
        if !authorized_by_user && !authorized_by_group {
            return Err(RegistryError::UnauthorizedPrincipal);
        }

        let repository = match request.requested_repository.as_deref() {
            Some(repository) => normalize_repository(repository)?,
            None => binding.default_repository.clone(),
        };
        if !binding.repository_allowlist.contains(&repository) {
            return Err(RegistryError::RepositoryNotAllowed);
        }

        let agent_mode = request
            .requested_agent_mode
            .unwrap_or(binding.default_agent_mode);
        if !binding.allowed_agent_modes.contains(&agent_mode) {
            return Err(RegistryError::AgentModeNotAllowed);
        }
        if !binding.write_policy.permits(request.requested_capability) {
            return Err(RegistryError::WriteNotAllowed);
        }

        let issue = request
            .linear_issue_identifier
            .as_deref()
            .map(parse_linear_issue_identifier)
            .transpose()?;
        if issue
            .as_ref()
            .is_some_and(|issue| issue.team_key != binding.linear_team_key)
        {
            return Err(RegistryError::IssueTeamMismatch);
        }

        Ok(ResolvedProjectContext {
            workspace_id: binding.workspace_id.clone(),
            channel_id: binding.channel_id.clone(),
            linear_team_id: binding.linear_team_id.clone(),
            linear_team_key: binding.linear_team_key.clone(),
            linear_project_id: binding.linear_project_id.clone(),
            repository,
            agent_mode,
            write_policy: binding.write_policy,
            budget_policy: binding.budget_policy,
            issue,
        })
    }

    pub fn binding_count(&self) -> usize {
        self.bindings.len()
    }
}

pub fn parse_linear_issue_identifier(value: &str) -> Result<LinearIssueRef, RegistryError> {
    let value = value.trim();
    let (team_key, number) = value
        .split_once('-')
        .ok_or(RegistryError::InvalidIssueIdentifier)?;
    validate_team_key(team_key).map_err(|_| RegistryError::InvalidIssueIdentifier)?;
    if number.is_empty()
        || number.starts_with('0')
        || !number.chars().all(|character| character.is_ascii_digit())
    {
        return Err(RegistryError::InvalidIssueIdentifier);
    }
    let number = number
        .parse::<u64>()
        .ok()
        .filter(|number| *number > 0)
        .ok_or(RegistryError::InvalidIssueIdentifier)?;
    Ok(LinearIssueRef {
        identifier: value.to_string(),
        team_key: team_key.to_string(),
        number,
    })
}

fn validate_binding(binding: &mut ChannelProjectBinding) -> Result<(), RegistryError> {
    for identifier in [
        &binding.workspace_id,
        &binding.channel_id,
        &binding.linear_team_id,
        &binding.linear_project_id,
        &binding.updated_by,
    ] {
        validate_stable_identifier(identifier)?;
    }
    validate_team_key(&binding.linear_team_key)?;
    DateTime::parse_from_rfc3339(&binding.updated_at)
        .map_err(|_| RegistryError::InvalidAuditTimestamp)?;

    if binding.repository_allowlist.is_empty()
        || binding.repository_allowlist.len() > MAX_REPOSITORIES_PER_BINDING
    {
        return Err(if binding.repository_allowlist.is_empty() {
            RegistryError::DefaultRepositoryNotAllowed
        } else {
            RegistryError::TooManyRepositories
        });
    }
    let normalized_repositories = binding
        .repository_allowlist
        .iter()
        .map(|repository| normalize_repository(repository))
        .collect::<Result<BTreeSet<_>, _>>()?;
    binding.repository_allowlist = normalized_repositories;
    binding.default_repository = normalize_repository(&binding.default_repository)?;
    if !binding
        .repository_allowlist
        .contains(&binding.default_repository)
    {
        return Err(RegistryError::DefaultRepositoryNotAllowed);
    }

    if binding.allowed_agent_modes.is_empty() {
        return Err(RegistryError::MissingAgentMode);
    }
    if !binding
        .allowed_agent_modes
        .contains(&binding.default_agent_mode)
    {
        return Err(RegistryError::DefaultAgentModeNotAllowed);
    }

    let principal_count = binding.allowed_user_ids.len() + binding.allowed_user_group_ids.len();
    if principal_count == 0 {
        return Err(RegistryError::MissingPrincipal);
    }
    if principal_count > MAX_PRINCIPALS_PER_BINDING {
        return Err(RegistryError::TooManyPrincipals);
    }
    for identifier in binding
        .allowed_user_ids
        .iter()
        .chain(&binding.allowed_user_group_ids)
    {
        validate_stable_identifier(identifier)?;
    }
    validate_budget(binding.budget_policy)
}

fn validate_budget(budget: BudgetPolicy) -> Result<(), RegistryError> {
    if budget.max_concurrent_runs == 0
        || budget.max_concurrent_runs > MAX_CONCURRENT_RUNS
        || budget.max_runtime_secs < 5
        || budget.max_runtime_secs > MAX_RUNTIME_SECS
        || budget.max_tokens == 0
        || budget.max_tokens > MAX_TOKENS
        || budget.max_spend_cents == 0
        || budget.max_spend_cents > MAX_SPEND_CENTS
        || budget.max_retries > MAX_RETRIES
    {
        return Err(RegistryError::InvalidBudget);
    }
    Ok(())
}

fn validate_stable_identifier(value: &str) -> Result<(), RegistryError> {
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_BYTES
        || value.chars().any(|character| {
            !character.is_ascii_alphanumeric() && !matches!(character, '-' | '_' | '.' | ':')
        })
    {
        return Err(RegistryError::InvalidIdentifier);
    }
    Ok(())
}

fn validate_team_key(value: &str) -> Result<(), RegistryError> {
    if !(2..=10).contains(&value.len())
        || !value
            .chars()
            .all(|character| character.is_ascii_uppercase() || character.is_ascii_digit())
        || !value
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_uppercase())
    {
        return Err(RegistryError::InvalidTeamKey);
    }
    Ok(())
}

fn normalize_repository(value: &str) -> Result<String, RegistryError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_BYTES
        || value.contains("://")
        || value.ends_with(".git")
        || value.matches('/').count() != 1
    {
        return Err(RegistryError::InvalidRepository);
    }
    let (owner, repository) = value
        .split_once('/')
        .ok_or(RegistryError::InvalidRepository)?;
    if owner.is_empty()
        || repository.is_empty()
        || owner.chars().any(|character| {
            !character.is_ascii_alphanumeric() && !matches!(character, '-' | '_' | '.')
        })
        || repository.chars().any(|character| {
            !character.is_ascii_alphanumeric() && !matches!(character, '-' | '_' | '.')
        })
    {
        return Err(RegistryError::InvalidRepository);
    }
    Ok(format!(
        "{}/{}",
        owner.to_ascii_lowercase(),
        repository.to_ascii_lowercase()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_document() -> serde_json::Value {
        serde_json::json!({
            "schema_version": 1,
            "bindings": [{
                "workspace_id": "T012345",
                "channel_id": "C012345",
                "linear_team_id": "eb8ab169-5afe-4b6f-9cab-3f2aa3e887dc",
                "linear_team_key": "DEN",
                "linear_project_id": "3abf3f94-6ce2-489d-a810-344f010aa068",
                "default_repository": "ORESoftware/ai-agent-bridge.rs",
                "repository_allowlist": [
                    "ORESoftware/ai-agent-bridge.rs",
                    "ORESoftware/ai-agent-coordinator.rs"
                ],
                "default_agent_mode": "both-parallel",
                "allowed_agent_modes": ["claude", "chatgpt", "both-parallel", "review"],
                "allowed_user_ids": ["U012345"],
                "allowed_user_group_ids": ["S012345"],
                "write_policy": "draft_pull_request",
                "budget_policy": {
                    "max_concurrent_runs": 2,
                    "max_runtime_secs": 900,
                    "max_tokens": 100000,
                    "max_spend_cents": 1000,
                    "max_retries": 2
                },
                "updated_by": "UADMIN",
                "updated_at": "2026-07-31T06:00:00Z"
            }]
        })
    }

    fn registry() -> SlackProjectRegistry {
        SlackProjectRegistry::from_json(&serde_json::to_vec(&valid_document()).unwrap()).unwrap()
    }

    fn request() -> ResolveRequest {
        ResolveRequest {
            workspace_id: "T012345".to_string(),
            channel_id: "C012345".to_string(),
            user_id: "U012345".to_string(),
            user_group_ids: BTreeSet::new(),
            requested_repository: None,
            requested_agent_mode: None,
            requested_capability: RequestedCapability::ReadOnly,
            linear_issue_identifier: Some("DEN-1042".to_string()),
        }
    }

    #[test]
    fn resolves_default_project_repository_mode_and_issue() {
        let resolved = registry().resolve(&request()).unwrap();
        assert_eq!(resolved.linear_team_key, "DEN");
        assert_eq!(resolved.repository, "oresoftware/ai-agent-bridge.rs");
        assert_eq!(resolved.agent_mode, AgentMode::BothParallel);
        assert_eq!(resolved.issue.unwrap().number, 1042);
    }

    #[test]
    fn rejects_duplicate_channel_bindings() {
        let mut document = valid_document();
        let duplicate = document["bindings"][0].clone();
        document["bindings"].as_array_mut().unwrap().push(duplicate);
        assert!(matches!(
            SlackProjectRegistry::from_json(&serde_json::to_vec(&document).unwrap()),
            Err(RegistryError::DuplicateBinding)
        ));
    }

    #[test]
    fn rejects_unmapped_and_unauthorized_requests() {
        let mut unmapped = request();
        unmapped.channel_id = "C999999".to_string();
        assert!(matches!(
            registry().resolve(&unmapped),
            Err(RegistryError::UnmappedChannel)
        ));

        let mut unauthorized = request();
        unauthorized.user_id = "U999999".to_string();
        assert!(matches!(
            registry().resolve(&unauthorized),
            Err(RegistryError::UnauthorizedPrincipal)
        ));
    }

    #[test]
    fn permits_explicit_authorized_user_groups() {
        let mut grouped = request();
        grouped.user_id = "U999999".to_string();
        grouped.user_group_ids.insert("S012345".to_string());
        assert!(registry().resolve(&grouped).is_ok());
    }

    #[test]
    fn enforces_repository_and_agent_mode_allowlists() {
        let mut bad_repository = request();
        bad_repository.requested_repository = Some("ORESoftware/k8s-cluster".to_string());
        assert!(matches!(
            registry().resolve(&bad_repository),
            Err(RegistryError::RepositoryNotAllowed)
        ));

        let mut allowed_repository = request();
        allowed_repository.requested_repository =
            Some("oresoftware/ai-agent-coordinator.rs".to_string());
        allowed_repository.requested_agent_mode = Some(AgentMode::Review);
        let resolved = registry().resolve(&allowed_repository).unwrap();
        assert_eq!(resolved.repository, "oresoftware/ai-agent-coordinator.rs");
        assert_eq!(resolved.agent_mode, AgentMode::Review);

        let mut bad_mode = request();
        bad_mode.requested_agent_mode = Some(AgentMode::BothSequential);
        assert!(matches!(
            registry().resolve(&bad_mode),
            Err(RegistryError::AgentModeNotAllowed)
        ));
    }

    #[test]
    fn enforces_write_policy_without_widening_repository_access() {
        let mut document = valid_document();
        document["bindings"][0]["write_policy"] = serde_json::json!("linear_only");
        let registry =
            SlackProjectRegistry::from_json(&serde_json::to_vec(&document).unwrap()).unwrap();

        let mut linear_write = request();
        linear_write.requested_capability = RequestedCapability::LinearWrite;
        assert!(registry.resolve(&linear_write).is_ok());

        let mut repository_write = request();
        repository_write.requested_capability = RequestedCapability::RepositoryWrite;
        assert!(matches!(
            registry.resolve(&repository_write),
            Err(RegistryError::WriteNotAllowed)
        ));
    }

    #[test]
    fn rejects_issue_identifiers_from_other_linear_teams() {
        let mut request = request();
        request.linear_issue_identifier = Some("ABC-10".to_string());
        assert!(matches!(
            registry().resolve(&request),
            Err(RegistryError::IssueTeamMismatch)
        ));
        assert!(matches!(
            parse_linear_issue_identifier("DEN-0"),
            Err(RegistryError::InvalidIssueIdentifier)
        ));
    }

    #[test]
    fn rejects_invalid_defaults_principals_budgets_and_unknown_fields() {
        let mut missing_default = valid_document();
        missing_default["bindings"][0]["default_repository"] =
            serde_json::json!("ORESoftware/not-allowed");
        assert!(matches!(
            SlackProjectRegistry::from_json(&serde_json::to_vec(&missing_default).unwrap()),
            Err(RegistryError::DefaultRepositoryNotAllowed)
        ));

        let mut missing_principal = valid_document();
        missing_principal["bindings"][0]["allowed_user_ids"] = serde_json::json!([]);
        missing_principal["bindings"][0]["allowed_user_group_ids"] = serde_json::json!([]);
        assert!(matches!(
            SlackProjectRegistry::from_json(&serde_json::to_vec(&missing_principal).unwrap()),
            Err(RegistryError::MissingPrincipal)
        ));

        let mut invalid_budget = valid_document();
        invalid_budget["bindings"][0]["budget_policy"]["max_spend_cents"] = serde_json::json!(0);
        assert!(matches!(
            SlackProjectRegistry::from_json(&serde_json::to_vec(&invalid_budget).unwrap()),
            Err(RegistryError::InvalidBudget)
        ));

        let mut unknown_field = valid_document();
        unknown_field["bindings"][0]["channel_name"] = serde_json::json!("oresoftware");
        assert!(matches!(
            SlackProjectRegistry::from_json(&serde_json::to_vec(&unknown_field).unwrap()),
            Err(RegistryError::Json(_))
        ));
    }
}
