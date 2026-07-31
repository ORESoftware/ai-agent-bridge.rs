# Bounded provider retries

Provider retries are disabled unless `AI_PROVIDER_RETRY_POLICY_JSON` explicitly enables them for a configured provider. This avoids introducing autonomous spend or duplicate ambiguous requests when upgrading an existing runner.

## Configuration

```json
{
  "guard_interval_ms": 1000,
  "providers": {
    "codex": {
      "max_retries": 2,
      "base_delay_ms": 250,
      "max_delay_ms": 5000,
      "max_total_delay_ms": 15000,
      "retryable_statuses": [408, 425, 429, 500, 502, 503, 504, 529],
      "retry_connect_errors": true,
      "retry_timeout_errors": false
    }
  }
}
```

The configuration is environment-only. It contains no credentials, prompts, repository paths, or URLs. Unknown providers, unknown fields, more than five retries, unsafe statuses, and delays outside the absolute bounds fail runner startup.

Status `409` is accepted only when an operator explicitly lists it for a provider whose API documents that outcome as transient. Authentication, permission, validation, redirect, oversized-response, and invalid-response failures cannot be configured as retryable.

## Transport ambiguity

Connect failures are retryable by default when retries are enabled because the request did not establish a provider connection. Request timeouts are not retryable by default: a provider may have processed an attempt even though the runner did not receive a response. An operator must explicitly enable timeout retries only for a provider/request contract with an independently reviewed idempotency guarantee.

Response-body stream failures and generic request-construction failures are never retried. The runner does not infer idempotency from a provider name.

## Retry-After and backoff

The provider client retains only redacted failure metadata:

- HTTP status;
- bounded `Retry-After` duration parsed from delta seconds or an HTTP date;
- a stable failure category such as rate-limited, overloaded, temporarily unavailable, or server error;
- transport category such as connect or timeout.

Raw error bodies, messages, URLs, headers, credentials, and prompts are discarded. `Retry-After` is capped by the configured maximum delay and total delay ceiling. Without a usable header, the runner uses deterministic 50–100% jitter over capped exponential backoff. The seed is the workflow, assignment, provider, and runner identity, so tests are deterministic and independent assignments do not synchronize their retries.

## Admission and accounting

The initial attempt must already have an accepted reservation. After a retry delay and immediately before another request starts, the runner atomically reserves:

- one retry;
- one provider call;
- the full conservative input allowance;
- the full maximum output allowance;
- token-rate cost and fixed call reserve;
- phase concurrency.

A rejected retry reservation starts no request. Failed-attempt reservations are not refunded because the provider may have consumed tokens or performed work. The successful attempt reports actual usage above its own reservation. Final failure reports elapsed time and retains every accepted attempt reservation.

The admission record remains the hard authority. Provider retry configuration cannot override its retry, call, token, cost, time, or concurrency ceilings.

## Cancellation and ownership guards

During provider requests and retry delays, the runner polls the durable admission and immutable workflow assignment at a bounded interval. It cancels the in-flight future or delay when:

- runner shutdown begins;
- admission is exhausted, completed, or cancelled;
- the assignment already submitted or is no longer pending;
- the bridge cannot prove the workflow guard;
- a retry reservation is rejected.

The outer Fiducia heartbeat remains active for the entire attempt/retry loop. Assignment-claim or file-lease renewal failure drops the loop immediately, discards output, cancels admission, and releases both grants best-effort.

Cancellation prevents further requests and output submission. It cannot prove that a remote provider stopped processing a request already accepted by that provider; therefore timeout retries remain disabled unless separately justified.

## Audit metadata

Submission metadata may contain only bounded retry facts:

- retry count;
- total configured delay spent;
- retry ordinal;
- delay source (`retry_after` or `exponential_jitter`);
- stable reason category and numeric HTTP status where applicable.

It never contains response bodies, prompts, provider URLs, tokens, secret identifiers, or provider-controlled strings.
