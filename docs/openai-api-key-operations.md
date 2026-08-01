# OpenAI API key operations

This runbook defines the credential boundary for the OpenAI provider used by the
AI agent runner. The OpenAI key is a server-side provider credential. It is not a
browser token, a ChatGPT connector credential, or a replacement for Browser MCP
OAuth 2.1.

## Ownership and scope

- Account owner: `alexander.d.mills@gmail.com`
- OpenAI API project: `oresoftware-agents`
- Runtime environment variable: `OPENAI_API_KEY`
- Recommended secret-manager path: `openai/oresoftware-agents/api-key`
- Intended consumer: the `fiducia-ai-agent-runner` workload only

Never put the key value in Git, Linear, GitHub issues or pull requests, ChatGPT,
container images, Kubernetes manifests, shell history, screenshots, logs, traces,
or provider configuration JSON.

## Account-owner provisioning

These steps must be completed interactively by the account owner while signed in
to the intended OpenAI Platform account and organization:

1. Verify the signed-in account and organization before changing billing or keys.
2. Create or select the dedicated `oresoftware-agents` API project.
3. Configure API billing or prepaid credits and conservative project-level usage
   controls.
4. Create a project-scoped secret key for the runner workload.
5. Copy the value directly into the approved secret manager.
6. Revoke unknown, unused, or previously exposed keys.

The key is displayed only at creation time. Do not route it through a chat or
issue as an intermediate clipboard.

## Provider configuration

`AI_PROVIDER_CONFIG_JSON` contains an environment-variable name, never a
credential value:

```json
[
  {
    "name": "openai",
    "protocol": "open_ai_responses",
    "base_url": "https://api.openai.com/v1/",
    "model": "YOUR_OPENAI_MODEL",
    "api_key_env": "OPENAI_API_KEY",
    "allowed_hosts": ["api.openai.com"]
  }
]
```

The runner fails during startup when the named environment variable is missing
or empty. Startup errors identify the variable name but do not print its value.
Provider HTTP failure bodies are not returned or logged by the adapter.

## Kubernetes injection contract

Use the cluster's approved External Secrets mechanism. The exact store name and
namespace are deployment-specific, but the resulting workload contract should
be equivalent to:

```yaml
apiVersion: external-secrets.io/v1beta1
kind: ExternalSecret
metadata:
  name: fiducia-ai-agent-provider-secrets
  namespace: fiducia
spec:
  refreshInterval: 1h
  secretStoreRef:
    kind: ClusterSecretStore
    name: cluster-secrets
  target:
    name: fiducia-ai-agent-provider-secrets
    creationPolicy: Owner
  data:
    - secretKey: OPENAI_API_KEY
      remoteRef:
        key: openai/oresoftware-agents/api-key
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: fiducia-ai-agent-runner
  namespace: fiducia
spec:
  template:
    spec:
      containers:
        - name: runner
          env:
            - name: OPENAI_API_KEY
              valueFrom:
                secretKeyRef:
                  name: fiducia-ai-agent-provider-secrets
                  key: OPENAI_API_KEY
```

Do not expose this Secret through a ConfigMap, public environment endpoint,
frontend bundle, debug dump, or cluster-wide shared environment injection.
Restrict workload identity and Kubernetes RBAC so unrelated pods cannot read it.

## Verification

Run repository checks before deployment:

```bash
python3 scripts/audit-provider-secrets.py
cargo test --locked --test provider_secret_contract
```

After secret injection, perform one bounded server-side request through the
runner/provider adapter. Use a small output-token limit and a non-sensitive
prompt. Verify only these facts:

- authentication succeeds;
- billing is active;
- the configured model is accessible to the project;
- the request ID and token usage are recorded without request headers or secret
  values;
- no provider response body is logged on failure.

Do not print the key, its prefix, a hash of the key, or a partially masked form as
part of verification. Presence is represented as a boolean or a successful
provider-client initialization only.

## Rotation

1. Create a replacement project-scoped key in the same OpenAI API project.
2. Update the secret-manager value at the stable path.
3. Force or wait for ExternalSecret reconciliation.
4. Restart or roll the runner workload so new processes read the replacement.
5. Run the bounded verification request.
6. Revoke the previous key only after the new key is confirmed healthy.
7. Record the rotation date, operator, workload, and verification result without
   recording either key.

## Emergency revocation

When exposure is suspected:

1. Revoke the affected key immediately in the OpenAI Platform.
2. Stop or scale down the consuming workload if it is repeatedly failing or may
   still be leaking credentials.
3. Remove the exposed value from the secret manager and replace it with a new key.
4. Search Git history, Actions logs, application logs, traces, Linear, and chat
   systems for the exposure location. Do not repeat the value in search notes.
5. Purge or rotate downstream artifacts that captured the value.
6. Restore the workload and run bounded verification.
7. Document cause and prevention controls.

## Browser MCP boundary

Browser MCP remains authenticated through its OAuth 2.1 and PKCE flow. The
OpenAI provider key must never be sent to a browser, custom connector, MCP client,
or end user. The runner uses the key only for outbound server-to-server calls to
an exact HTTPS allowlisted provider host.
