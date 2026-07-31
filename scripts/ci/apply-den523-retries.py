#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one match, found {count}: {old!r}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


def splice(path: str, start_marker: str, end_marker: str, replacement: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    start = text.find(start_marker)
    if start < 0:
        raise RuntimeError(f"{path}: start marker missing: {start_marker!r}")
    end = text.find(end_marker, start)
    if end < 0:
        raise RuntimeError(f"{path}: end marker missing: {end_marker!r}")
    file.write_text(text[:start] + replacement + text[end:], encoding="utf-8")


def patch_providers() -> None:
    replace_once(
        "src/providers.rs",
        "\n#[derive(Clone, Debug, Deserialize, Serialize)]\npub struct ProviderConfig",
        "\ninclude!(\"providers/failure.rs\");\n\n#[derive(Clone, Debug, Deserialize, Serialize)]\npub struct ProviderConfig",
    )
    splice(
        "src/providers.rs",
        "#[derive(Debug, thiserror::Error)]\npub enum ProviderError {",
        "\n\nimpl ProviderConfig",
        "",
    )
    splice(
        "src/providers.rs",
        "        let response = builder\n",
        "        let request_id = request_id(response.headers());\n",
        """        let response = builder
            .json(&prepared.body)
            .send()
            .await
            .map_err(|error| ProviderError::Transport {
                kind: classify_transport(&error),
            })?;
        let status = response.status();
        let retry_after = parse_retry_after(response.headers());
        if !status.is_success() {
            let bytes = read_bounded(response, self.config.raw.max_response_bytes).await?;
            let failure_kind = parse_provider_failure(self.config.raw.protocol, &bytes);
            return Err(ProviderError::HttpStatus {
                status,
                retry_after,
                failure_kind,
            });
        }
""",
    )
    replace_once(
        "src/providers.rs",
        "        let chunk = chunk.map_err(|_| ProviderError::Transport)?;",
        "        let chunk = chunk.map_err(|_| ProviderError::Transport {\n            kind: ProviderTransportKind::ResponseBody,\n        })?;",
    )


def patch_provider_tests() -> None:
    path = Path("src/providers/http_tests.rs")
    text = path.read_text(encoding="utf-8")
    text = text.replace(
        "ProviderError::HttpStatus(StatusCode::TEMPORARY_REDIRECT)",
        "ProviderError::HttpStatus {\n            status: StatusCode::TEMPORARY_REDIRECT,\n            ..\n        }",
    )
    text = text.replace(
        "ProviderError::HttpStatus(StatusCode::TOO_MANY_REQUESTS)",
        "ProviderError::HttpStatus {\n            status: StatusCode::TOO_MANY_REQUESTS,\n            ..\n        }",
    )
    text = text.replace(
        "Err(ProviderError::Transport)\n    ));",
        "Err(ProviderError::Transport { .. })\n    ));",
    )
    marker = "#[tokio::test]\nasync fn response_limit_applies_to_content_length_and_streamed_bodies()"
    if text.count(marker) != 1:
        raise RuntimeError("provider retry metadata test insertion marker missing")
    test = """#[tokio::test]
async fn retry_metadata_is_bounded_and_redacted() {
    let overloaded = StatusCode::from_u16(529).unwrap();
    let state = MockState::json(json!({
        "error": {
            "type": "overloaded_error",
            "message": "provider-controlled secret body"
        }
    }))
    .with_status(overloaded)
    .with_header("retry-after", "3");
    let (base, _) = spawn(state).await;
    let client = ProviderClient::with_api_key(
        config(
            "claude",
            ProviderProtocol::AnthropicMessages,
            format!("{base}v1/"),
            "claude-test",
            1024,
            5,
        ),
        TEST_SECRET,
    )
    .unwrap();
    let error = client.execute(&request()).await.unwrap_err();
    assert_eq!(error.http_status(), Some(overloaded));
    assert_eq!(error.retry_after(), Some(Duration::from_secs(3)));
    assert_eq!(error.failure_kind(), Some(ProviderFailureKind::Overloaded));
    let rendered = error.to_string();
    assert!(!rendered.contains(TEST_SECRET));
    assert!(!rendered.contains("provider-controlled"));
    assert!(!rendered.contains("secret body"));
}

"""
    path.write_text(text.replace(marker, test + marker, 1), encoding="utf-8")


def patch_admission() -> None:
    replacement = """    pub(crate) async fn reserve_call(
        &self,
        workflow_id: &str,
        provider_agent_key: &str,
        request: &ProviderRequest,
        concurrency: u8,
    ) -> Result<(AdmissionRecord, UsageReservation), AdmissionClientError> {
        self.reserve_attempt(
            workflow_id,
            provider_agent_key,
            request,
            concurrency,
            false,
        )
        .await
    }

    pub(crate) async fn reserve_retry(
        &self,
        workflow_id: &str,
        provider_agent_key: &str,
        request: &ProviderRequest,
        concurrency: u8,
    ) -> Result<(AdmissionRecord, UsageReservation), AdmissionClientError> {
        self.reserve_attempt(
            workflow_id,
            provider_agent_key,
            request,
            concurrency,
            true,
        )
        .await
    }

    async fn reserve_attempt(
        &self,
        workflow_id: &str,
        provider_agent_key: &str,
        request: &ProviderRequest,
        concurrency: u8,
        is_retry: bool,
    ) -> Result<(AdmissionRecord, UsageReservation), AdmissionClientError> {
        let pricing = self
            .pricing
            .get(provider_agent_key)
            .ok_or(AdmissionClientError::UnpricedUsage)?;
        let input_tokens = conservative_input_tokens(request);
        let output_tokens = u64::from(request.max_output_tokens);
        if pricing.max_context_tokens > 0 && input_tokens > pricing.max_context_tokens {
            return Err(AdmissionClientError::InputTooLarge);
        }
        let token_cost = price_usage(pricing, input_tokens, output_tokens);
        let cost_micro_usd = token_cost.saturating_add(pricing.fixed_call_reserve_micro_usd);
        let admission = self
            .report_usage(
                workflow_id,
                provider_agent_key,
                UsageDelta {
                    input_tokens,
                    output_tokens,
                    cost_micro_usd,
                    retries: if is_retry { 1 } else { 0 },
                    provider_calls: 1,
                    concurrency,
                    ..UsageDelta::default()
                },
            )
            .await?;
        Ok((
            admission,
            UsageReservation {
                input_tokens,
                output_tokens,
                cost_micro_usd,
            },
        ))
    }

"""
    splice(
        "src/runner/admission.rs",
        "    pub(crate) async fn reserve_call(\n",
        "    pub(crate) async fn report_response(\n",
        replacement,
    )
    replace_once(
        "src/runner/admission.rs",
        "    async fn get(&self, workflow_id: &str) -> Result<AdmissionRecord, AdmissionClientError> {\n",
        """    pub(crate) async fn ensure_active(
        &self,
        workflow_id: &str,
    ) -> Result<AdmissionRecord, AdmissionClientError> {
        validate_active(self.get(workflow_id).await?)
    }

    async fn get(&self, workflow_id: &str) -> Result<AdmissionRecord, AdmissionClientError> {
""",
    )


def patch_runner_header() -> None:
    replace_once(
        "src/runner/mod.rs",
        "mod heartbeat;\nmod work;",
        "mod heartbeat;\nmod retry;\nmod retry_execution;\nmod work;",
    )
    replace_once(
        "src/runner/mod.rs",
        "use tokio::sync::Semaphore;",
        "use tokio::sync::{watch, Semaphore};",
    )
    replace_once(
        "src/runner/mod.rs",
        "use heartbeat::{run_with_heartbeat, HeartbeatOutcome};\n",
        "use heartbeat::{run_with_heartbeat, HeartbeatOutcome};\nuse retry::{RetryPolicies, RetryPolicy};\nuse retry_execution::RetryRun;\n",
    )
    replace_once(
        "src/runner/mod.rs",
        "    admission: AdmissionControl,\n    providers: Arc<Vec<ProviderWorker>>,",
        "    admission: AdmissionControl,\n    retry_policies: RetryPolicies,\n    shutdown_tx: watch::Sender<bool>,\n    providers: Arc<Vec<ProviderWorker>>,
",
    )
    replace_once(
        "src/runner/mod.rs",
        "        let admission = AdmissionControl::from_env(&providers, &claims)?;\n        Ok(Self {\n",
        "        let admission = AdmissionControl::from_env(&providers, &claims)?;\n        let retry_policies = RetryPolicies::from_env(&providers)?;\n        let (shutdown_tx, _) = watch::channel(false);\n        Ok(Self {\n",
    )
    replace_once(
        "src/runner/mod.rs",
        "            admission,\n            providers: Arc::new(providers),",
        "            admission,\n            retry_policies,\n            shutdown_tx,\n            providers: Arc::new(providers),",
    )
    replace_once(
        "src/runner/mod.rs",
        "                    info!(\"provider runner shutdown requested\");\n                    break;",
        "                    info!(\"provider runner shutdown requested\");\n                    let _ = self.shutdown_tx.send(true);\n                    break;",
    )
    replace_once(
        "src/runner/mod.rs",
        "                let max_output_tokens = self.max_output_tokens;\n                tokio::spawn(async move {",
        """                let max_output_tokens = self.max_output_tokens;
                let retry_policy = self.retry_policies.policy(&provider.config.name);
                let retry_guard_interval = self.retry_policies.guard_interval();
                let shutdown = self.shutdown_tx.subscribe();
                tokio::spawn(async move {""",
    )
    replace_once(
        "src/runner/mod.rs",
        "                        claims,\n                        lease_safety_margin_ms,\n                        max_output_tokens,\n",
        "                        claims,\n                        retry_policy,\n                        retry_guard_interval,\n                        shutdown,\n                        lease_safety_margin_ms,\n                        max_output_tokens,\n",
    )
    replace_once(
        "src/runner/mod.rs",
        "    claims: ClaimConfig,\n    lease_safety_margin_ms: u64,\n    max_output_tokens: u32,\n",
        "    claims: ClaimConfig,\n    retry_policy: RetryPolicy,\n    retry_guard_interval: Duration,\n    mut shutdown: watch::Receiver<bool>,\n    lease_safety_margin_ms: u64,\n    max_output_tokens: u32,\n",
    )


def patch_runner_execution() -> None:
    replacement = """    let file_fencing_token = file_lease.as_ref().map(|handle| handle.fencing_token);
    let heartbeat_ttl_ms = claim
        .iter()
        .chain(file_lease.iter())
        .map(|handle| handle.ttl_ms)
        .min();
    let retry_seed = format!(
        "{}/{}/{}/{}",
        workflow.plan.id, assignment.ordinal, provider.config.name, claims.instance_id
    );
    let retry_future = retry_execution::execute(
        &retry_policy,
        retry_guard_interval,
        &retry_seed,
        &admission,
        &bridge,
        &provider,
        &workflow,
        &assignment,
        &request,
        reservation,
        &mut shutdown,
    );
    let started = Instant::now();
    let execution = if let Some(ttl_ms) = heartbeat_ttl_ms {
        let renewal_bridge = bridge.clone();
        let renewal_claim = claim.clone();
        let renewal_file_lease = file_lease.clone();
        let claim_owner = claims.owner.clone();
        let provider_agent_key = provider.config.name.clone();
        run_with_heartbeat(
            retry_future,
            ttl_ms,
            lease_safety_margin_ms,
            move || {
                let bridge = renewal_bridge.clone();
                let claim = renewal_claim.clone();
                let file_lease = renewal_file_lease.clone();
                let claim_owner = claim_owner.clone();
                let provider_agent_key = provider_agent_key.clone();
                async move {
                    if let Some(handle) = claim.as_ref() {
                        bridge.renew_lease(handle, &claim_owner).await?;
                    }
                    if let Some(handle) = file_lease.as_ref() {
                        bridge.renew_lease(handle, &provider_agent_key).await?;
                    }
                    Ok::<_, bridge::BridgeClientError>(())
                }
            },
        )
        .await
    } else {
        HeartbeatOutcome::Completed {
            output: retry_future.await,
            renewals: 0,
        }
    };
    let elapsed_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);

    let (content, mut meta, renewals) = match execution {
        HeartbeatOutcome::Completed {
            output: RetryRun::Success(success),
            renewals,
        } => {
            let retry_meta =
                retry_execution::audits_json(&success.audits, success.total_delay);
            if let Err(error) = admission
                .report_response(
                    &workflow.plan.id,
                    &provider.config.name,
                    &success.response.usage,
                    elapsed_ms,
                    success.reservation,
                )
                .await
            {
                warn!(
                    workflow_id = %workflow.plan.id,
                    assignment_ordinal = assignment.ordinal,
                    agent_key = %provider.config.name,
                    %error,
                    "provider output exceeded or could not prove the admitted budget; discarded"
                );
                release_file_lease(
                    &bridge,
                    file_lease.as_ref(),
                    &provider.config.name,
                    &workflow,
                )
                .await;
                release_claim(&bridge, claim.as_ref(), &claims, &workflow, &assignment).await;
                return;
            }
            let mut meta = success_meta(
                &success.response.provider,
                &success.response.model,
                provider.config.protocol,
                success.response.request_id.as_deref(),
                success.response.usage,
                file_fencing_token,
            );
            if let Some(object) = meta.as_object_mut() {
                object.insert("provider_retries".into(), retry_meta);
            }
            (success.response.text, meta, renewals)
        }
        HeartbeatOutcome::Completed {
            output: RetryRun::FinalFailure {
                error,
                audits,
                total_delay,
            },
            renewals,
        } => {
            let error = error.to_string();
            if let Err(accounting_error) = admission
                .report_failure(&workflow.plan.id, &provider.config.name, elapsed_ms)
                .await
            {
                warn!(
                    workflow_id = %workflow.plan.id,
                    assignment_ordinal = assignment.ordinal,
                    agent_key = %provider.config.name,
                    %accounting_error,
                    "failed provider call could not be accounted; workflow output discarded"
                );
                release_file_lease(
                    &bridge,
                    file_lease.as_ref(),
                    &provider.config.name,
                    &workflow,
                )
                .await;
                release_claim(&bridge, claim.as_ref(), &claims, &workflow, &assignment).await;
                return;
            }
            let mut meta = failure_meta(
                &provider.config.name,
                &provider.config.model,
                provider.config.protocol,
                &error,
                file_fencing_token,
            );
            if let Some(object) = meta.as_object_mut() {
                object.insert(
                    "provider_retries".into(),
                    retry_execution::audits_json(&audits, total_delay),
                );
            }
            (
                format!("Provider execution failed: {error}"),
                meta,
                renewals,
            )
        }
        HeartbeatOutcome::Completed {
            output: RetryRun::Aborted {
                reason,
                audits,
                total_delay,
            },
            renewals,
        } => {
            let reason = retry_execution::abort_reason(reason);
            warn!(
                workflow_id = %workflow.plan.id,
                assignment_ordinal = assignment.ordinal,
                agent_key = %provider.config.name,
                retry_count = audits.len(),
                retry_delay_ms = total_delay.as_millis(),
                lease_renewals = renewals,
                cancellation_reason = reason,
                "provider attempt or retry cancelled; output discarded"
            );
            let _ = admission.cancel(&workflow.plan.id, reason).await;
            release_file_lease(
                &bridge,
                file_lease.as_ref(),
                &provider.config.name,
                &workflow,
            )
            .await;
            release_claim(&bridge, claim.as_ref(), &claims, &workflow, &assignment).await;
            return;
        }
        HeartbeatOutcome::LeaseLost { error, renewals } => {
            warn!(
                workflow_id = %workflow.plan.id,
                assignment_ordinal = assignment.ordinal,
                agent_key = %provider.config.name,
                file_fencing_token = ?file_fencing_token,
                claim_fencing_token = ?claim.as_ref().map(|handle| handle.fencing_token),
                successful_renewals = renewals,
                %error,
                "assignment or file lease heartbeat failed; provider output discarded"
            );
            let _ = admission
                .cancel(&workflow.plan.id, "Fiducia heartbeat failed")
                .await;
            release_file_lease(
                &bridge,
                file_lease.as_ref(),
                &provider.config.name,
                &workflow,
            )
            .await;
            release_claim(&bridge, claim.as_ref(), &claims, &workflow, &assignment).await;
            return;
        }
    };

"""
    splice(
        "src/runner/mod.rs",
        "    let file_fencing_token = file_lease.as_ref().map(|handle| handle.fencing_token);\n",
        "    annotate_heartbeat(&mut meta, claim.is_some(), file_lease.is_some(), renewals);\n",
        replacement,
    )


def patch_misc() -> None:
    replace_once(
        "src/runner/bridge.rs",
        "    #[allow(dead_code)]\n    pub(crate) async fn get_workflow(\n",
        "    pub(crate) async fn get_workflow(\n",
    )
    replace_once(
        ".cli-flags.toml",
        '"AI_PROVIDER_CONFIG_JSON", "AI_PROVIDER_PRICING_JSON",',
        '"AI_PROVIDER_CONFIG_JSON", "AI_PROVIDER_PRICING_JSON", "AI_PROVIDER_RETRY_POLICY_JSON",',
    )
    replace_once(
        "src/runner/retry.rs",
        "\n    pub(crate) fn max_retries(&self) -> u8 {\n        self.max_retries\n    }\n",
        "",
    )


patch_providers()
patch_provider_tests()
patch_admission()
patch_runner_header()
patch_runner_execution()
patch_misc()
