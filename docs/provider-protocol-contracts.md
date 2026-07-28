# Provider protocol contracts and normalized usage

The provider adapter supports five configured model families through four wire protocols:

| Model family | Protocol | Relative endpoint |
|---|---|---|
| OpenAI / Codex | OpenAI Responses | `responses` |
| Claude | Anthropic Messages | `messages` |
| Gemini | Gemini `generateContent` | `models/{model}:generateContent` |
| Kimi | OpenAI-compatible chat | `chat/completions` |
| Qwen | OpenAI-compatible chat | `chat/completions` |

Provider base URLs are validated once, must use HTTPS except for loopback tests, and must exactly match the configured host allowlist. Redirects are disabled. Credentials are sent only in protocol-specific headers and never in URLs.

## Canonical usage object

Successful responses expose usage in a protocol-independent shape:

```json
{
  "input_tokens": 12,
  "output_tokens": 7,
  "total_tokens": 19,
  "raw": {
    "promptTokenCount": 12,
    "candidatesTokenCount": 7,
    "totalTokenCount": 19
  }
}
```

The canonical fields are populated from:

- OpenAI Responses: `usage.input_tokens`, `usage.output_tokens`, and `usage.total_tokens`;
- Anthropic Messages: `usage.input_tokens` and `usage.output_tokens`;
- Gemini: `usageMetadata.promptTokenCount`, `candidatesTokenCount`, and `totalTokenCount`;
- OpenAI-compatible chat: `usage.prompt_tokens`, `completion_tokens`, and `total_tokens`, with input/output aliases accepted.

When a provider omits total tokens, the adapter uses the saturating sum of input and output tokens. When the provider omits its usage object entirely, `usage` remains `null`; policy-accounted execution must reject that output rather than guess.

The raw usage object is retained for auditing and future provider-specific fields. It must not contain credentials, prompts, or response text.

## Error and body limits

The adapter:

- refuses redirects;
- returns only a status/category for non-success responses and does not include provider-controlled response bodies;
- applies the configured byte limit using both `Content-Length` and streamed chunk accounting;
- maps timeout, connection, and response-stream failures to a generic transport error;
- rejects invalid JSON and successful responses that do not contain usable text.

The mock matrix exercises the real HTTP client against loopback servers for all provider families and verifies exact paths, headers, request bodies, request IDs, response parsing, usage normalization, redirects, redaction, timeouts, and response-size enforcement.
