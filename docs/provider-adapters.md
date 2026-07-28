# Provider protocol adapters

This module supplies the hardened HTTP protocol layer for model workers. It does
not schedule workflows and does not store credentials. A worker reads an
assignment from the bridge, acquires its Fiducia lease, calls one configured
provider through `ProviderClient`, posts the result to the workflow submission
endpoint, and releases the lease.

## Supported protocols

| `protocol` | Intended providers | Base URL contract | Relative endpoint |
|---|---|---|---|
| `open_ai_responses` | OpenAI / Codex models | API base ending in `/v1/` | `responses` |
| `anthropic_messages` | Claude | API base ending in `/v1/` | `messages` |
| `gemini_generate_content` | Gemini | API base ending in `/v1beta/` | `models/{model}:generateContent` |
| `open_ai_compatible_chat` | Kimi, Qwen, and compatible gateways | Compatible API base ending in `/v1/` | `chat/completions` |

The base URL deliberately includes the provider API prefix. This supports
regional and workspace-specific Qwen/Kimi-compatible endpoints without
hard-coding provider hostnames in the bridge. Copy the provider's documented
regional API base exactly and retain its trailing slash.

## Configuration

`AI_PROVIDER_CONFIG_JSON` is an array. It contains environment-variable names,
never credential values:

```json
[
  {
    "name": "openai-codex",
    "protocol": "open_ai_responses",
    "base_url": "https://api.openai.com/v1/",
    "model": "YOUR_OPENAI_MODEL",
    "api_key_env": "OPENAI_API_KEY",
    "allowed_hosts": ["api.openai.com"]
  },
  {
    "name": "claude",
    "protocol": "anthropic_messages",
    "base_url": "https://api.anthropic.com/v1/",
    "model": "YOUR_CLAUDE_MODEL",
    "api_key_env": "ANTHROPIC_API_KEY",
    "allowed_hosts": ["api.anthropic.com"]
  },
  {
    "name": "gemini",
    "protocol": "gemini_generate_content",
    "base_url": "https://generativelanguage.googleapis.com/v1beta/",
    "model": "YOUR_GEMINI_MODEL",
    "api_key_env": "GEMINI_API_KEY",
    "allowed_hosts": ["generativelanguage.googleapis.com"]
  },
  {
    "name": "qwen-us",
    "protocol": "open_ai_compatible_chat",
    "base_url": "https://dashscope-us.aliyuncs.com/compatible-mode/v1/",
    "model": "YOUR_QWEN_MODEL",
    "api_key_env": "DASHSCOPE_API_KEY",
    "allowed_hosts": ["dashscope-us.aliyuncs.com"]
  }
]
```

Kimi can use any operator-approved OpenAI-compatible endpoint by supplying its
API base, model identifier, credential environment variable, and exact host.
Alibaba Model Studio also exposes Kimi models through its regional compatible
chat endpoint.

## Rust usage

```rust
use ai_agent_bridge::providers::{
    parse_provider_configs, ProviderClient, ProviderRequest,
};

let raw = std::env::var("AI_PROVIDER_CONFIG_JSON")?;
let configs = parse_provider_configs(&raw)?;
let clients = configs
    .into_iter()
    .map(ProviderClient::from_config)
    .collect::<Result<Vec<_>, _>>()?;

let response = clients[0]
    .execute(&ProviderRequest {
        prompt: "Review this proposed Rust patch.".into(),
        system: Some("Return findings and a corrected patch.".into()),
        max_output_tokens: 4096,
    })
    .await?;
```

## Security contract

- Secrets come only from `api_key_env`; serialized config cannot contain an API
  key field.
- Remote endpoints require HTTPS. Plain HTTP is accepted only for localhost or a
  loopback IP used by integration tests.
- The base URL host must exactly match an allowlisted host.
- Redirects are disabled so credentials cannot be forwarded to a second host.
- Base URLs containing user information, queries, or fragments are rejected.
- Prompts plus system instructions are capped at 1 MiB.
- Responses are streamed into a bounded buffer; the default cap is 4 MiB and the
  absolute configurable ceiling is 32 MiB.
- Provider failure bodies are not returned or logged by this module.
- OpenAI Responses requests set `store: false` by default.
- Gemini model identifiers are constrained before they are inserted into a URL
  path segment.

## Deliberate boundaries

This module does not yet:

- poll workflow assignments;
- manage per-provider concurrency or budgets;
- renew Fiducia leases;
- retry provider calls;
- stream partial model output;
- execute model-requested tools;
- post the final result back to `/workflows/{id}/submissions`.

Those runner concerns remain separate from the provider HTTP protocol layer so
credentials and provider failures cannot destabilize the conversation bus.
