pub fn router() -> Router {
    Router::new()
        .route("/workflow-policy", get(describe_policy))
        .route("/workflow-policy/explain", post(explain_policy))
        .layer(DefaultBodyLimit::max(MAX_POLICY_BODY_BYTES))
}

async fn describe_policy() -> Json<serde_json::Value> {
    Json(json!({
        "ok": true,
        "policy_version": POLICY_VERSION,
        "modes": ["single", "sequential", "competitive", "consensus"],
        "coordination_protocols": [
            "direct",
            "sequential_handoff",
            "independent_candidates",
            "blind_candidates_with_reviewer_reveal",
            "reviewer_consensus",
            "adversarial_review_required"
        ],
        "execution_targets": [
            "standard_workflow",
            "blind_competition",
            "adversarial_review"
        ],
        "dispositions": ["execute", "require_human_approval", "queue", "deny"],
        "provider_ranking": [
            "availability",
            "historical_quality_bps_desc",
            "health_score_bps_desc",
            "recent_error_rate_bps_asc",
            "p95_latency_ms_asc",
            "estimated_cost_micro_usd_asc",
            "agent_key_asc"
        ],
        "hard_caps": {
            "max_providers": ABS_MAX_PROVIDERS,
            "max_rounds": ABS_MAX_ROUNDS,
            "max_wall_clock_ms": ABS_MAX_WALL_CLOCK_MS,
            "max_input_tokens": ABS_MAX_INPUT_TOKENS,
            "max_output_tokens": ABS_MAX_OUTPUT_TOKENS,
            "max_retries": ABS_MAX_RETRIES,
            "max_concurrency": ABS_MAX_CONCURRENCY,
            "max_cost_micro_usd": ABS_MAX_COST_MICRO_USD,
        }
    }))
}

async fn explain_policy(Json(request): Json<PolicyRequest>) -> Response {
    match evaluate(&request) {
        Ok(decision) => Json(json!({
            "ok": decision.allowed,
            "dry_run": true,
            "creates_workflow": false,
            "creates_provider_work": false,
            "decision": decision,
        }))
        .into_response(),
        Err(detail) => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "dry_run": true,
                "creates_workflow": false,
                "creates_provider_work": false,
                "error": "invalid_policy_request",
                "detail": detail,
                "policy_version": POLICY_VERSION,
            })),
        )
            .into_response(),
    }
}
