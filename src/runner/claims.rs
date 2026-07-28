use serde_json::{json, Value};

use crate::assignment_claims::claim_path;

const DEFAULT_CLAIM_REPOSITORY: &str = "fiducia-cloud/ai-agent-assignment-claims";
const DEFAULT_CLAIM_TTL_MS: u64 = 60_000;
const MAX_CLAIM_TTL_MS: u64 = 86_400_000;
const MAX_INSTANCE_ID_BYTES: usize = 80;

#[derive(Clone, Debug)]
pub(crate) struct ClaimConfig {
    pub enabled: bool,
    pub replica_count: usize,
    pub instance_id: String,
    pub owner: String,
    pub repository: String,
    pub ttl_ms: u64,
}

impl ClaimConfig {
    pub(crate) fn from_env() -> anyhow::Result<Self> {
        let enabled = env_bool("AI_AGENT_RUNNER_DISTRIBUTED_CLAIMS", false)?;
        let replica_count = env_usize("AI_AGENT_RUNNER_REPLICA_COUNT", 1).max(1);
        if replica_count > 1 && !enabled {
            anyhow::bail!(
                "AI_AGENT_RUNNER_REPLICA_COUNT>1 requires AI_AGENT_RUNNER_DISTRIBUTED_CLAIMS=true"
            );
        }

        let instance_id = std::env::var("AI_AGENT_RUNNER_INSTANCE_ID")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("HOSTNAME")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "single-replica".to_string());
        validate_instance_id(&instance_id)?;
        if enabled && instance_id == "single-replica" {
            anyhow::bail!(
                "distributed claims require AI_AGENT_RUNNER_INSTANCE_ID or a non-empty HOSTNAME"
            );
        }

        let repository = std::env::var("AI_AGENT_RUNNER_ASSIGNMENT_CLAIM_REPOSITORY")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_CLAIM_REPOSITORY.to_string())
            .trim()
            .to_ascii_lowercase();
        validate_repository(&repository)?;
        let ttl_ms = env_u64("AI_AGENT_RUNNER_ASSIGNMENT_CLAIM_TTL_MS", DEFAULT_CLAIM_TTL_MS)
            .clamp(1, MAX_CLAIM_TTL_MS);
        let owner = format!("runner/{instance_id}");
        if owner.len() > 120 {
            anyhow::bail!("runner assignment-claim owner exceeds 120 bytes");
        }

        Ok(Self {
            enabled,
            replica_count,
            instance_id,
            owner,
            repository,
            ttl_ms,
        })
    }

    pub(crate) fn path(&self, workflow_id: &str, assignment_ordinal: usize) -> String {
        claim_path(workflow_id, assignment_ordinal)
    }

    pub(crate) fn metadata(
        &self,
        workflow_id: &str,
        assignment_ordinal: usize,
        fencing_token: u64,
    ) -> Value {
        json!({
            "repository": self.repository,
            "paths": [self.path(workflow_id, assignment_ordinal)],
            "owner": self.owner,
            "instance_id": self.instance_id,
            "assignment_ordinal": assignment_ordinal,
            "fencing_token": fencing_token,
            "ttl_ms": self.ttl_ms,
        })
    }
}

fn validate_instance_id(value: &str) -> anyhow::Result<()> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > MAX_INSTANCE_ID_BYTES
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        anyhow::bail!(
            "AI_AGENT_RUNNER_INSTANCE_ID must be 1-{MAX_INSTANCE_ID_BYTES} characters using letters, digits, '.', '_' or '-'"
        );
    }
    Ok(())
}

fn validate_repository(value: &str) -> anyhow::Result<()> {
    let parts = value.split('/').collect::<Vec<_>>();
    if parts.len() != 2
        || parts.iter().any(|part| {
            part.is_empty()
                || !part
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character))
        })
    {
        anyhow::bail!("assignment claim repository must be canonical owner/repo");
    }
    Ok(())
}

fn env_bool(key: &str, default: bool) -> anyhow::Result<bool> {
    match std::env::var(key)
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        None | Some("") => Ok(default),
        Some("1" | "true" | "yes" | "on") => Ok(true),
        Some("0" | "false" | "no" | "off") => Ok(false),
        Some(_) => anyhow::bail!("{key} must be a boolean"),
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_path_and_metadata_are_deterministic() {
        let config = ClaimConfig {
            enabled: true,
            replica_count: 2,
            instance_id: "runner-a".into(),
            owner: "runner/runner-a".into(),
            repository: DEFAULT_CLAIM_REPOSITORY.into(),
            ttl_ms: 60_000,
        };
        assert_eq!(
            config.path("workflow-1", 3),
            "workflows/workflow-1/assignments/3"
        );
        let meta = config.metadata("workflow-1", 3, 42);
        assert_eq!(meta["owner"], "runner/runner-a");
        assert_eq!(meta["fencing_token"], 42);
    }

    #[test]
    fn instance_ids_are_bounded_and_path_safe() {
        assert!(validate_instance_id("pod-0.example").is_ok());
        assert!(validate_instance_id("pod/0").is_err());
        assert!(validate_instance_id("bad\nvalue").is_err());
    }
}
