pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/workflows/{workflow_id}/admission",
            get(get_admission).post(admit_workflow),
        )
        .route(
            "/workflows/{workflow_id}/admission/usage",
            post(report_usage),
        )
        .route(
            "/workflows/{workflow_id}/admission/complete",
            post(complete_admission),
        )
        .route(
            "/workflows/{workflow_id}/admission/cancel",
            post(cancel_admission),
        )
        .layer(DefaultBodyLimit::max(MAX_ADMISSION_BODY_BYTES))
        .layer(from_fn(request_timeout))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .layer(tower_http::catch_panic::CatchPanicLayer::new())
        .with_state(state)
}

async fn request_timeout(request: axum::extract::Request, next: Next) -> Response {
    match tokio::time::timeout(Duration::from_secs(30), next.run(request)).await {
        Ok(response) => response,
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(json!({"ok":false,"error":"request_timeout"})),
        )
            .into_response(),
    }
}

async fn get_admission(
    State(state): State<Arc<AppState>>,
    Path(workflow_id): Path<String>,
) -> Response {
    match load_admission(&state, &workflow_id) {
        Ok(Some(admission)) => Json(json!({
            "ok": true,
            "created": false,
            "admission": admission,
        }))
        .into_response(),
        Ok(None) => AdmissionFailure::new(
            StatusCode::NOT_FOUND,
            "admission_not_found",
            "workflow admission is missing",
        )
        .into_response(),
        Err(error) => ApiError(error).into_response(),
    }
}

async fn admit_workflow(
    State(state): State<Arc<AppState>>,
    Path(workflow_id): Path<String>,
    Json(request): Json<AdmitReq>,
) -> Response {
    match create_admission(&state, &workflow_id, request) {
        Ok((admission, created)) => Json(json!({
            "ok": true,
            "created": created,
            "admission": admission,
        }))
        .into_response(),
        Err(error) => error.into_response(),
    }
}

async fn report_usage(
    State(state): State<Arc<AppState>>,
    Path(workflow_id): Path<String>,
    Json(request): Json<UsageReq>,
) -> Response {
    match record_usage(&state, &workflow_id, &request.updated_by, request.delta) {
        Ok(admission) => Json(json!({"ok":true,"admission":admission})).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn complete_admission(
    State(state): State<Arc<AppState>>,
    Path(workflow_id): Path<String>,
    Json(request): Json<TerminalReq>,
) -> Response {
    match transition(
        &state,
        &workflow_id,
        &request.updated_by,
        AdmissionStatus::Completed,
        request.reason.as_deref(),
    ) {
        Ok(admission) => Json(json!({"ok":true,"admission":admission})).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn cancel_admission(
    State(state): State<Arc<AppState>>,
    Path(workflow_id): Path<String>,
    Json(request): Json<TerminalReq>,
) -> Response {
    match transition(
        &state,
        &workflow_id,
        &request.updated_by,
        AdmissionStatus::Cancelled,
        request.reason.as_deref(),
    ) {
        Ok(admission) => Json(json!({"ok":true,"admission":admission})).into_response(),
        Err(error) => error.into_response(),
    }
}
