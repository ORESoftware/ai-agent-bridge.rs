# Benefactor project routing

The `#benefactor-cc` Slack binding is a project-scoped draft-PR gateway. It is
not a general GitHub credential and does not bypass repository review policy.

The default target remains `benefactor-cc/benefactor-cc-mcp-server.rs`. A user
must name another repository explicitly and request a repository-write
capability before the registry will consider a different target. Linear issue
context is optional at the registry boundary; when supplied, it must be a valid
`DEN-*` identifier and is preserved in the resolved project context. Operational
agent workflows should attach the relevant Benefactor issue whenever work is
issue-driven.

## Reviewed repositories

The binding permits draft-PR work in:

- `benefactor-cc/benefactor-cc-mcp-server.rs`
- `benefactor-cc/backend.rs`
- `benefactor-cc/benefactor-sync`
- `benefactor-cc/benefactor-interfaces`
- `benefactor-cc/benefactor-e2e`
- `benefactor-cc/benefactor-automations`
- `benefactor-cc/benefactor-sendgrid-outreach`
- `benefactor-cc/benefactor-monorepo`
- `benefactor-cc/benefactor-lib`
- `benefactor-cc/benefactor-clients`
- `benefactor-cc/benefactor-cli`
- `benefactor-cc/.github`
- `ORESoftware/benefactor.cc`
- `ORESoftware/k8s-cluster`

The bridge retains the existing `draft_pull_request` policy, principal
allowlist, agent-mode allowlist, concurrency/runtime/token/spend budgets, and
Linear project binding. Every resulting branch and pull request remains subject
to the target repository's own `agents.md`, CI, review, and merge requirements.

## Deliberate exclusions

`benefactor-cc/benefactor-cc.github.io` is generated production output and must
be changed through the canonical Astro source in `ORESoftware/benefactor.cc`.
`benefactor-cc/benfactor-cc` is the preserved misspelled legacy site. Neither is
an agent write target.

Unknown repositories, URLs, `.git` suffixes, path traversal, subpaths, and other
projects' repositories fail closed. Supplied malformed issue identifiers or
issue identifiers from a non-`DEN` team also fail closed.
