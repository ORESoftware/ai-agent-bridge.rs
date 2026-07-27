pub fn router(state: Arc<AppState>) -> Router {
    let body_limit = state.config.max_http_body_bytes;
    Router::new()
        .route("/blind-workflows", post(create_blind_competition))
        .route("/blind-workflows/{workflow_id}", get(get_blind_competition))
        .route(
            "/blind-workflows/{workflow_id}/submissions",
            post(submit_blind_competition),
        )
        .route(
            "/blind-workflows/{workflow_id}/reveal",
            post(reveal_blind_competition),
        )
        .layer(from_fn_with_state(state.clone(), auth))
        .layer(DefaultBodyLimit::max(body_limit))
        .layer(from_fn(request_timeout))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .layer(tower_http::catch_panic::CatchPanicLayer::new())
        .with_state(state)
}

async fn request_timeout(req: axum::extract::Request, next: Next) -> Response {
    match tokio::time::timeout(Duration::from_secs(30), next.run(req)).await {
        Ok(response) => response,
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(json!({ "ok": false, "error": "request_timeout" })),
        )
            .into_response(),
    }
}

async fn auth(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    if let Some(expected) = &state.config.api_auth_bearer {
        let presented = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        let authorized = presented
            .map(|value| {
                crate::config::constant_time_eq(value.as_bytes(), expected.as_bytes())
            })
            .unwrap_or(false);
        if !authorized {
            return ApiError(BridgeError::Unauthorized).into_response();
        }
    }
    next.run(req).await
}

async fn create_blind_competition(
    State(state): State<Arc<AppState>>,
    viewer: Option<Extension<AuthenticatedAdapter>>,
    Json(req): Json<CreateBlindCompetitionReq>,
) -> ApiResult {
    let title = normalize_required_text(&req.title, "title", MAX_TITLE_BYTES)?;
    let prompt = normalize_required_text(
        &req.prompt,
        "prompt",
        state.config.max_content_bytes,
    )?;
    let created_by = normalize_agent_key(&req.created_by)?;
    validate_json_size(
        &req.meta,
        state.config.max_content_bytes,
        "blind competition meta",
    )?;

    if req.worker_agent_keys.len() < 2 {
        return Err(BridgeError::BadRequest(
            "blind competition requires at least two workers".into(),
        )
        .into());
    }
    if req.worker_agent_keys.len() > MAX_BLIND_WORKERS {
        return Err(BridgeError::CapacityExceeded {
            what: "blind competition workers",
            limit: MAX_BLIND_WORKERS,
        }
        .into());
    }

    let registered = registered_agents_by_key(&state);
    let mut seen = BTreeSet::new();
    let mut workers = Vec::new();
    for requested in &req.worker_agent_keys {
        let agent_key = normalize_agent_key(requested)?;
        if !seen.insert(agent_key.clone()) {
            return Err(BridgeError::BadRequest(format!(
                "duplicate blind competition worker '{agent_key}'"
            ))
            .into());
        }
        if !registered.contains_key(&agent_key) {
            return Err(BridgeError::AgentNotFound(agent_key).into());
        }
        workers.push(BlindWorker {
            ordinal: workers.len(),
            agent_key,
        });
    }

    let reviewer_agent_key = normalize_agent_key(&req.reviewer_agent_key)?;
    if seen.contains(&reviewer_agent_key) {
        return Err(BridgeError::BadRequest(
            "blind competition reviewer must be distinct from every worker".into(),
        )
        .into());
    }
    if !registered.contains_key(&reviewer_agent_key) {
        return Err(BridgeError::AgentNotFound(reviewer_agent_key).into());
    }

    let id = crate::types::new_id();
    let channel = blind_channel(&id)?;
    let plan = BlindCompetitionPlan {
        version: 1,
        id: id.clone(),
        channel: channel.clone(),
        title,
        prompt,
        created_by: created_by.clone(),
        created_at: now_ts(),
        workers,
        reviewer_agent_key: reviewer_agent_key.clone(),
        meta: req.meta,
    };

    state
        .create_or_get_channel(&channel, &plan.title, &created_by)
        .await?;
    for worker in &plan.workers {
        state.join(&channel, &worker.agent_key, MemberRole::Member)?;
    }
    state.join(&channel, &reviewer_agent_key, MemberRole::Member)?;
    insert_blind_context(
        &state,
        &channel,
        BLIND_PLAN_CONTEXT_KEY,
        serde_json::to_value(&plan).map_err(|_| {
            BridgeError::BadRequest("blind competition plan is not serializable".into())
        })?,
        &created_by,
    )?;
    state.post_message(
        &channel,
        &created_by,
        Role::System,
        &format!(
            "Blind competition {} opened for {} workers.",
            plan.id,
            plan.workers.len()
        ),
        json!({
            "kind": "blind_competition_created",
            "workflow_id": plan.id,
            "worker_count": plan.workers.len(),
            "reviewer_agent_key": plan.reviewer_agent_key,
        }),
    )?;

    let view = build_view(&state, &plan, viewer_key(viewer.as_ref()))?;
    Ok(Json(json!({ "ok": true, "workflow": view })))
}

async fn get_blind_competition(
    State(state): State<Arc<AppState>>,
    Path(workflow_id): Path<String>,
    viewer: Option<Extension<AuthenticatedAdapter>>,
) -> ApiResult {
    let plan = load_plan(&state, &workflow_id)?;
    let view = build_view(&state, &plan, viewer_key(viewer.as_ref()))?;
    Ok(Json(json!({ "ok": true, "workflow": view })))
}

async fn submit_blind_competition(
    State(state): State<Arc<AppState>>,
    Path(workflow_id): Path<String>,
    viewer: Option<Extension<AuthenticatedAdapter>>,
    Json(req): Json<SubmitBlindCompetitionReq>,
) -> ApiResult {
    let Some(Extension(identity)) = viewer else {
        return Err(BridgeError::Unauthorized.into());
    };
    let requested_agent_key = normalize_agent_key(&req.agent_key)?;
    if requested_agent_key != identity.agent_key {
        return Err(BridgeError::Unauthorized.into());
    }
    let content = normalize_required_text(
        &req.content,
        "blind submission content",
        state.config.max_content_bytes,
    )?;
    validate_json_size(
        &req.meta,
        state.config.max_content_bytes,
        "blind submission meta",
    )?;

    let plan = load_plan(&state, &workflow_id)?;
    if load_reveal(&state, &plan.channel)?.is_some() {
        return Err(BridgeError::BadRequest(
            "blind competition submissions are closed after reveal".into(),
        )
        .into());
    }
    let worker = plan
        .workers
        .iter()
        .find(|worker| worker.agent_key == identity.agent_key)
        .ok_or_else(|| {
            BridgeError::BadRequest(format!(
                "agent '{}' is not a worker in blind competition '{}'",
                identity.agent_key, plan.id
            ))
        })?;
    let context_key = format!("{BLIND_SUBMISSION_CONTEXT_PREFIX}{}", worker.ordinal);
    let submission = BlindSubmission {
        workflow_id: plan.id.clone(),
        assignment_ordinal: worker.ordinal,
        agent_key: identity.agent_key.clone(),
        content,
        meta: req.meta,
        submitted_at: now_ts(),
    };
    insert_blind_context(
        &state,
        &plan.channel,
        &context_key,
        serde_json::to_value(&submission).map_err(|_| {
            BridgeError::BadRequest("blind submission is not serializable".into())
        })?,
        &identity.agent_key,
    )?;

    state.post_message(
        &plan.channel,
        &identity.agent_key,
        Role::System,
        &format!("Blind candidate {} submitted.", worker.ordinal),
        json!({
            "kind": "blind_submission_received",
            "workflow_id": plan.id,
            "assignment_ordinal": worker.ordinal,
            "agent_key": identity.agent_key,
            "candidate_content_in_channel": false,
        }),
    )?;

    let view = build_view(&state, &plan, Some(identity.agent_key.as_str()))?;
    Ok(Json(json!({ "ok": true, "workflow": view })))
}

async fn reveal_blind_competition(
    State(state): State<Arc<AppState>>,
    Path(workflow_id): Path<String>,
    viewer: Option<Extension<AuthenticatedAdapter>>,
) -> ApiResult {
    let Some(Extension(identity)) = viewer else {
        return Err(BridgeError::Unauthorized.into());
    };
    let plan = load_plan(&state, &workflow_id)?;
    if identity.agent_key != plan.reviewer_agent_key {
        return Err(BridgeError::Unauthorized.into());
    }
    let submissions = load_submissions(&state, &plan.channel)?;
    if submissions.len() != plan.workers.len() {
        return Err(BridgeError::BadRequest(format!(
            "reviewer must wait for all {} worker submissions; received {}",
            plan.workers.len(),
            submissions.len()
        ))
        .into());
    }
    let reveal = BlindReveal {
        workflow_id: plan.id.clone(),
        reviewer_agent_key: identity.agent_key.clone(),
        revealed_at: now_ts(),
    };
    insert_blind_context(
        &state,
        &plan.channel,
        BLIND_REVEAL_CONTEXT_KEY,
        serde_json::to_value(&reveal).map_err(|_| {
            BridgeError::BadRequest("blind reveal is not serializable".into())
        })?,
        &identity.agent_key,
    )?;
    state.post_message(
        &plan.channel,
        &identity.agent_key,
        Role::System,
        "Blind candidate set revealed to authorized workflow readers.",
        json!({
            "kind": "blind_competition_revealed",
            "workflow_id": plan.id,
            "reviewer_agent_key": identity.agent_key,
            "candidate_count": submissions.len(),
        }),
    )?;

    let view = build_view(&state, &plan, Some(identity.agent_key.as_str()))?;
    Ok(Json(json!({ "ok": true, "workflow": view })))
}
