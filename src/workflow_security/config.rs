impl WorkflowSecurity {
    pub fn from_env(
        global_bearer: Option<String>,
        max_body_bytes: usize,
    ) -> anyhow::Result<Arc<Self>> {
        let json = std::env::var(CONFIG_ENV).ok();
        Self::from_optional_json(global_bearer, json.as_deref(), max_body_bytes)
    }

    pub fn from_json(
        global_bearer: Option<String>,
        json: &str,
        max_body_bytes: usize,
    ) -> anyhow::Result<Arc<Self>> {
        Self::from_optional_json(global_bearer, Some(json), max_body_bytes)
    }

    fn from_optional_json(
        global_bearer: Option<String>,
        json: Option<&str>,
        max_body_bytes: usize,
    ) -> anyhow::Result<Arc<Self>> {
        if let Some(global) = &global_bearer {
            HeaderValue::from_str(&format!("Bearer {global}"))
                .map_err(|_| anyhow::anyhow!("API_AUTH_BEARER is not a valid HTTP bearer"))?;
        }

        let document = match json.map(str::trim).filter(|value| !value.is_empty()) {
            Some(json) => serde_json::from_str::<CredentialDocument>(json)
                .map_err(|error| anyhow::anyhow!("{CONFIG_ENV} is invalid JSON: {error}"))?,
            None => CredentialDocument {
                credentials: Vec::new(),
            },
        };
        if document.credentials.len() > MAX_CREDENTIALS {
            anyhow::bail!("{CONFIG_ENV} contains more than {MAX_CREDENTIALS} credentials");
        }

        let mut token_ids = BTreeSet::new();
        let mut tokens = BTreeSet::new();
        let mut credentials = Vec::new();
        for input in document.credentials.into_iter().filter(|item| item.enabled) {
            let token_id = validate_text("token_id", input.token_id, MAX_KEY_BYTES)?;
            let agent_key = validate_text("agent_key", input.agent_key, MAX_KEY_BYTES)?;
            let token = validate_text("token", input.token, MAX_TOKEN_BYTES)?;
            if global_bearer.as_deref() == Some(token.as_str()) {
                anyhow::bail!(
                    "workflow credential '{token_id}' must not reuse API_AUTH_BEARER material"
                );
            }
            if !token_ids.insert(token_id.clone()) {
                anyhow::bail!("duplicate workflow credential token_id '{token_id}'");
            }
            if !tokens.insert(token.clone()) {
                anyhow::bail!("duplicate workflow credential token material");
            }
            if input.scopes.is_empty() || input.scopes.len() > MAX_SCOPES {
                anyhow::bail!(
                    "workflow credential '{token_id}' must contain 1..={MAX_SCOPES} scopes"
                );
            }
            let mut scopes = BTreeSet::new();
            for scope in input.scopes {
                let scope = scope.trim().to_ascii_lowercase();
                if !KNOWN_SCOPES.contains(&scope.as_str()) {
                    anyhow::bail!("workflow credential '{token_id}' has unknown scope '{scope}'");
                }
                scopes.insert(scope);
            }
            credentials.push(Credential {
                token_id,
                token,
                agent_key,
                scopes,
            });
        }

        Ok(Arc::new(Self {
            global_bearer,
            credentials,
            max_body_bytes: max_body_bytes.max(1),
        }))
    }

    pub fn authenticate_principal(&self, token: &str) -> Option<AuthenticatedPrincipal> {
        if self.is_admin_token(token) {
            return Some(AuthenticatedPrincipal::Operator);
        }
        self.authenticate(token)
            .map(AuthenticatedPrincipal::Adapter)
    }

    pub fn authentication_required(&self) -> bool {
        self.global_bearer.is_some() || self.scoped_mode()
    }

    pub(crate) fn authenticate(&self, token: &str) -> Option<AuthenticatedAdapter> {
        let mut matched = None;
        for credential in &self.credentials {
            let equal = crate::config::constant_time_eq(
                token.as_bytes(),
                credential.token.as_bytes(),
            );
            if equal && matched.is_none() {
                matched = Some(AuthenticatedAdapter {
                    token_id: credential.token_id.clone(),
                    agent_key: credential.agent_key.clone(),
                    scopes: credential.scopes.clone(),
                });
            }
        }
        matched
    }

    pub(crate) fn is_admin_token(&self, token: &str) -> bool {
        self.global_bearer
            .as_deref()
            .map(|expected| {
                crate::config::constant_time_eq(token.as_bytes(), expected.as_bytes())
            })
            .unwrap_or(false)
    }

    pub(crate) fn scoped_mode(&self) -> bool {
        !self.credentials.is_empty()
    }
}

fn validate_text(name: &str, value: String, max: usize) -> anyhow::Result<String> {
    let value = value.trim().to_string();
    if value.is_empty() || value.len() > max || value.chars().any(char::is_control) {
        anyhow::bail!("workflow credential {name} must be 1..={max} printable bytes");
    }
    Ok(value)
}
