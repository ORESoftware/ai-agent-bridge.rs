# alex-main-agent browser and registry tests

Tracking issue: `DEN-1298`

This test surface proves that the reviewed Slack channel registry is enforced by the production Rust `SlackProjectRegistry` resolver. It is not a mock Slack page and it is not a production HTTP service.

## Components

- `config/alex-main-agent.channels.json` — thirteen manifest-backed project channels plus the separately reviewed `#oresoftware` control-plane pilot.
- `src/bin/alex-main-agent-registry-browser.rs` — loopback-only diagnostic server that loads the real registry and exposes a minimal policy-resolution form.
- `tests/alex_main_agent_registry.rs` — Rust integration tests for registry parsing and core authorization boundaries.
- `tests/browser/specs/registry.spec.mjs` — Playwright scenarios executed in Chromium.

## Security boundary

The diagnostic server:

- refuses non-loopback bind addresses;
- rejects non-loopback and malformed loopback `Host` authorities, including empty, non-numeric, and out-of-range ports;
- rejects cross-site `Sec-Fetch-Site` requests and any `Origin` that does not exactly match the request's loopback authority;
- accepts at most 16 KiB request bodies;
- accepts at most a 1 MiB regular-file registry and rejects symbolic-link registry paths;
- emits restrictive Content Security Policy, COEP, COOP, CORP, Permissions Policy, referrer, framing, MIME-sniffing, and cache headers on successful and rejected responses;
- stores no Slack signing secret, bot token, Linear key, GitHub token, or provider credential;
- returns stable public error codes instead of internal policy details.

The production Slack command process independently enforces the same 1 MiB regular-file registry boundary before parsing.

## Local run

Use the pinned Rust toolchain and a supported Node runtime:

```bash
cargo build --locked --bin alex-main-agent-registry-browser

npm --prefix tests/browser install --ignore-scripts --no-audit --no-fund
npx --prefix tests/browser playwright install chromium

ALEX_MAIN_AGENT_REGISTRY_PATH="$PWD/config/alex-main-agent.channels.json" \
ALEX_MAIN_AGENT_PROBE_ADDR=127.0.0.1:8160 \
  target/debug/alex-main-agent-registry-browser
```

In another shell:

```bash
ALEX_MAIN_AGENT_PROBE_URL=http://127.0.0.1:8160 \
  npm --prefix tests/browser test
```

Do not bind the diagnostic to a public, container-wide, or cluster-wide interface. It is designed only for local and CI verification.

## Browser scenarios

Chromium verifies:

1. restrictive browser security headers;
2. successful Hypesiege resolution through the real Rust registry;
3. rejection of the misspelled Daedalus channel;
4. rejection of an unauthorized Slack principal;
5. rejection of a repository escape attempt;
6. rejection of a Linear issue from another team;
7. rejection of a non-loopback Host header with hardened error headers;
8. rejection of malformed loopback Host authorities and invalid ports;
9. rejection of cross-site and mismatched-loopback-origin requests before policy evaluation;
10. rejection of an oversized JSON request body.

The Rust binary's focused unit tests additionally prove strict loopback authority parsing, same-origin matching, fetch-metadata admission, and symbolic-link registry rejection.

## GitHub Actions

The `alex-main-agent registry boundary` workflow:

1. checks out without persisted credentials;
2. materializes only the exact pinned optional dependency metadata required for non-Postgres resolution;
3. validates the Rust diagnostic with formatting, warnings-denied Clippy, and focused tests;
4. installs the exact Playwright test version with lifecycle scripts disabled;
5. installs Chromium;
6. starts the probe on `127.0.0.1`;
7. verifies that all 14 bindings loaded while the remote manifest audit remains scoped to the thirteen project-owned entries;
8. runs the browser suite with one worker and retained failure traces;
9. terminates the probe and prints its log on failure.

The repository-wide workflows continue to own actionlint, provider-secret auditing, all-target Rust validation, PostgreSQL restart durability, cargo-audit, images, and flags-2-env checks. Their private shared-schema credential boundary is tracked separately in issue `#84`.
