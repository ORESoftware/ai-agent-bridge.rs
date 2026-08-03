const MAX_REGISTRY_BYTES: u64 = 1_048_576;

impl App {
    fn new(config: Config) -> Result<Self> {
        use std::io::Read as _;

        let metadata = fs::metadata(&config.registry_path)
            .map_err(|_| Error::Config("unable to inspect Slack project registry".into()))?;
        if !metadata.is_file() {
            return Err(Error::Config(
                "Slack project registry path must resolve to a regular file".into(),
            ));
        }
        let file = fs::File::open(&config.registry_path)
            .map_err(|_| Error::Config("unable to open Slack project registry".into()))?;
        let mut bytes = Vec::with_capacity(metadata.len().min(MAX_REGISTRY_BYTES) as usize);
        file.take(MAX_REGISTRY_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Config("unable to read Slack project registry".into()))?;
        if bytes.len() as u64 > MAX_REGISTRY_BYTES {
            return Err(Error::Config(
                "Slack project registry exceeds the maximum size".into(),
            ));
        }
        let registry = SlackProjectRegistry::from_json(&bytes)
            .map_err(|_| Error::Config("invalid Slack project registry".into()))?;
        let document = serde_json::from_slice::<SlackProjectRegistryDocument>(&bytes)
            .map_err(|_| Error::Config("invalid Slack project registry".into()))?;
        let bindings = document
            .bindings
            .into_iter()
            .map(|binding| {
                (
                    (binding.workspace_id.clone(), binding.channel_id.clone()),
                    binding,
                )
            })
            .collect();
        fs::create_dir_all(&config.state_dir).map_err(|_| Error::Journal)?;
        let client = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .user_agent("fiducia-slack-command/0.1")
            .build()
            .map_err(|_| Error::Config("unable to initialize HTTP client".into()))?;
        let capacity = Arc::new(Semaphore::new(config.max_concurrent_runs));
        Ok(Self {
            config,
            client,
            registry,
            bindings,
            capacity,
        })
    }

    async fn groups(
        &self,
        team_id: &str,
        channel_id: &str,
        user_id: &str,
    ) -> Result<BTreeSet<String>> {
        let binding = self
            .bindings
            .get(&(team_id.to_string(), channel_id.to_string()))
            .ok_or(Error::Policy)?;
        if binding.allowed_user_ids.contains(user_id) || binding.allowed_user_group_ids.is_empty() {
            return Ok(BTreeSet::new());
        }
        let response = self
            .client
            .get(self.config.slack_url("usergroups.list")?)
            .bearer_auth(&self.config.bot_token)
            .query(&[("include_users", "true")])
            .send()
            .await
            .map_err(|_| Error::Slack)?;
        let body = read_bounded(response, MAX_SLACK_RESPONSE_BYTES)
            .await
            .ok_or(Error::Slack)?;
        let response =
            serde_json::from_slice::<UsergroupsResponse>(&body).map_err(|_| Error::Slack)?;
        if !response.ok {
            return Err(Error::Slack);
        }
        Ok(response
            .usergroups
            .into_iter()
            .filter(|group| group.users.iter().any(|user| user == user_id))
            .map(|group| group.id)
            .collect())
    }

    async fn resolve(
        &self,
        request: &RunRequest,
    ) -> Result<crate::slack_project_bindings::ResolvedProjectContext> {
        let groups = self
            .groups(&request.team_id, &request.channel_id, &request.user_id)
            .await?;
        self.registry
            .resolve(&ResolveRequest {
                workspace_id: request.team_id.clone(),
                channel_id: request.channel_id.clone(),
                user_id: request.user_id.clone(),
                user_group_ids: groups,
                requested_repository: request.repository.clone(),
                requested_agent_mode: Some(request.provider.mode()),
                requested_capability: request.capability,
                linear_issue_identifier: request.linear_issue.clone(),
            })
            .map_err(|_| Error::Policy)
    }

    async fn command_binding(&self, command: &SlashCommand) -> Result<ChannelProjectBinding> {
        let groups = self
            .groups(&command.team_id, &command.channel_id, &command.user_id)
            .await?;
        self.registry
            .resolve(&ResolveRequest {
                workspace_id: command.team_id.clone(),
                channel_id: command.channel_id.clone(),
                user_id: command.user_id.clone(),
                user_group_ids: groups,
                requested_repository: None,
                requested_agent_mode: Some(command.provider().mode()),
                requested_capability: RequestedCapability::ReadOnly,
                linear_issue_identifier: None,
            })
            .map_err(|_| Error::Policy)?;
        self.bindings
            .get(&(command.team_id.clone(), command.channel_id.clone()))
            .cloned()
            .ok_or(Error::Policy)
    }

    fn claim(&self, request: &RunRequest) -> Result<bool> {
        let path = self.config.state_dir.join(&request.run_id);
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        }
        match options.open(path) {
            Ok(mut file) => {
                let record = json!({
                    "run_id": request.run_id,
                    "source_key": request.source_key,
                    "created_at": Utc::now().to_rfc3339()
                });
                writeln!(file, "{record}").map_err(|_| Error::Journal)?;
                file.sync_data().map_err(|_| Error::Journal)?;
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(_) => Err(Error::Journal),
        }
    }

    async fn context(&self, channel_id: &str, count: usize) -> Result<Vec<ContextMessage>> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let response = self
            .client
            .get(self.config.slack_url("conversations.history")?)
            .bearer_auth(&self.config.bot_token)
            .query(&[
                ("channel", channel_id.to_string()),
                ("limit", count.saturating_mul(4).min(100).to_string()),
            ])
            .send()
            .await
            .map_err(|_| Error::Slack)?;
        let body = read_bounded(response, MAX_REMOTE_RESPONSE_BYTES)
            .await
            .ok_or(Error::Slack)?;
        let response =
            serde_json::from_slice::<HistoryResponse>(&body).map_err(|_| Error::Slack)?;
        if !response.ok {
            return Err(Error::Slack);
        }
        let mut total = 0;
        let mut messages = Vec::new();
        for message in response.messages {
            if message.bot_id.is_some()
                || message.subtype.is_some()
                || message.text.trim().is_empty()
            {
                continue;
            }
            let text = truncate(message.text.trim(), MAX_CONTEXT_MESSAGE_BYTES);
            if total + text.len() > MAX_CONTEXT_TOTAL_BYTES {
                break;
            }
            total += text.len();
            messages.push(ContextMessage {
                user_id: message.user,
                ts: message.ts,
                text,
            });
            if messages.len() == count {
                break;
            }
        }
        messages.reverse();
        Ok(messages)
    }

    async fn open_modal(
        &self,
        command: &SlashCommand,
        binding: &ChannelProjectBinding,
    ) -> Result<()> {
        let metadata = serde_json::to_string(&ModalMetadata {
            provider: command.provider(),
            team_id: command.team_id.clone(),
            channel_id: command.channel_id.clone(),
            user_id: command.user_id.clone(),
        })
        .map_err(|_| Error::Slack)?;
        let response = self
            .client
            .post(self.config.slack_url("views.open")?)
            .bearer_auth(&self.config.bot_token)
            .json(&json!({
                "trigger_id": command.trigger_id,
                "view": modal(command.provider(), binding, &metadata, self.config.context_messages)
            }))
            .send()
            .await
            .map_err(|_| Error::Slack)?;
        slack_ok(response).await.map(|_| ())
    }

    async fn create_workflow(
        &self,
        request: &RunRequest,
        resolved: &crate::slack_project_bindings::ResolvedProjectContext,
        context: &[ContextMessage],
    ) -> Result<String> {
        let agent = self.config.agent_key(request.provider);
        let url = Url::parse(&self.config.bridge_url)
            .and_then(|base| base.join("workflows"))
            .map_err(|_| Error::Bridge)?;
        let mut http = self.client.post(url);
        if let Some(token) = &self.config.bridge_bearer {
            http = http.bearer_auth(token);
        }
        let response = http
            .json(&json!({
                "title": format!("{} Slack task {}", request.provider.label(), request.run_id),
                "prompt": agent_prompt(request, resolved, context),
                "created_by": agent,
                "mode": "single",
                "agent_keys": [agent],
                "worker_count": 1,
                "meta": {
                    "source": "slack_slash_command",
                    "run_id": request.run_id,
                    "repository": resolved.repository,
                    "slack_team_id": request.team_id,
                    "slack_channel_id": request.channel_id,
                    "slack_user_id": request.user_id,
                    "linear_project_id": resolved.linear_project_id,
                    "linear_run_project_id": self.config.linear_run_project_id,
                    "linear_issue": resolved.issue.as_ref().map(|issue| issue.identifier.as_str()),
                    "action": request.action,
                    "context_message_count": context.len()
                }
            }))
            .send()
            .await
            .map_err(|_| Error::Bridge)?;
        let status = response.status();
        let body = read_bounded(response, MAX_REMOTE_RESPONSE_BYTES)
            .await
            .ok_or(Error::Bridge)?;
        if !status.is_success() {
            let error_code = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|value| value.get("error").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| "unknown".to_string());
            warn!(
                run_id = %request.run_id,
                status = %status,
                error_code,
                "bridge rejected Slack workflow creation"
            );
            return Err(Error::Bridge);
        }
        let response =
            serde_json::from_slice::<WorkflowResponse>(&body).map_err(|_| Error::Bridge)?;
        if response.workflow.plan.assignments.len() != 1
            || response.workflow.plan.assignments[0].agent_key != agent
        {
            return Err(Error::Bridge);
        }
        Ok(response.workflow.plan.id)
    }

    async fn create_job(
        &self,
        request: &RunRequest,
        resolved: &crate::slack_project_bindings::ResolvedProjectContext,
        context: &[ContextMessage],
        workflow_id: &str,
    ) -> Result<String> {
        let (org, repo) = resolved
            .repository
            .split_once('/')
            .ok_or(Error::Coordinator)?;
        let url = Url::parse(&self.config.coordinator_url)
            .and_then(|base| base.join("v1/jobs"))
            .map_err(|_| Error::Coordinator)?;
        let mut http = self.client.post(url).header(
            "idempotency-key",
            format!("slack-command:{}", request.run_id),
        );
        if let Some(token) = &self.config.coordinator_bearer {
            http = http.bearer_auth(token);
        }
        let response = http
            .json(&json!({
                "org": org,
                "repo": repo,
                "task_type": "slack_agent_run",
                "priority": 100,
                "max_attempts": 3,
                "budget_usd": (resolved.budget_policy.max_spend_cents as f64) / 100.0,
                "payload": task_payload(&self.config, request, resolved, context, workflow_id)
            }))
            .send()
            .await
            .map_err(|_| Error::Coordinator)?;
        let status = response.status();
        let body = read_bounded(response, MAX_REMOTE_RESPONSE_BYTES)
            .await
            .ok_or(Error::Coordinator)?;
        if !status.is_success() {
            return Err(Error::Coordinator);
        }
        Ok(serde_json::from_slice::<JobResponse>(&body)
            .map_err(|_| Error::Coordinator)?
            .job
            .id)
    }

    async fn post_status(
        &self,
        request: &RunRequest,
        resolved: &crate::slack_project_bindings::ResolvedProjectContext,
        context_count: usize,
        workflow_id: &str,
        job_id: &str,
    ) -> Result<String> {
        let response = self
            .client
            .post(self.config.slack_url("chat.postMessage")?)
            .bearer_auth(&self.config.bot_token)
            .json(&json!({
                "channel": request.channel_id,
                "text": format!(
                    ":large_blue_circle: *{} work dispatched*\nRun: `{}`\nCoordinator job: `{}`\nBridge workflow: `{}`\nRepository: `{}`\nOwning Linear project: `{}`\nRun queue project: `{}`\nContext: {} latest non-bot channel messages\nWrite policy: `{}`",
                    request.provider.label(),
                    request.run_id,
                    job_id,
                    workflow_id,
                    resolved.repository,
                    resolved.linear_project_id,
                    self.config.linear_run_project_id,
                    context_count,
                    write_policy(resolved.write_policy)
                ),
                "unfurl_links": false,
                "unfurl_media": false
            }))
            .send()
            .await
            .map_err(|_| Error::Slack)?;
        slack_ok(response).await?.ts.ok_or(Error::Slack)
    }
}
