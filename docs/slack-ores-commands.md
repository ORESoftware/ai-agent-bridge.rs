# alex-main-agent Slack commands

Tracking issues: `DEN-1041`, `DEN-1298`

The `fiducia-slack-command` process accepts six reviewed command names. The `/ores-*` names are canonical; `/x-*` and `/my-*` are workspace convenience aliases that use the same provider, authorization, routing, budgets, and write policy.

```text
/ores-claude [task]
/ores-chatgpt [task]
/x-claude [task]
/x-chatgpt [task]
/my-claude [task]
/my-chatgpt [task]
```

## How to invoke the app

Run a command from the message composer of an authorized project channel. Examples:

```text
/ores-chatgpt investigate DEN-1298 and report the remaining activation blockers
/x-claude review the current pull request and fix actionable CI errors
/my-chatgpt
```

A command with text takes the fast path. A command without text opens a modal with these selections:

- task;
- action: implement, investigate, review, plan, or triage;
- repository from the channel's reviewed allowlist;
- optional Linear issue identifier;
- write scope bounded by channel policy;
- recent channel context: 0, 5, 10, or 20 messages.

The default is the five latest non-bot messages in the channel. Custom slash commands are channel-level; Slack does not invoke them inside message threads. Run the command in the channel composer, then use the resulting run-status thread for follow-up.

If none of the six commands appears in Slack autocomplete, the installed app configuration is stale or incomplete. Apply `slack-app/manifest.yaml` to app `A0BMBAMM5NJ`, reinstall the app to workspace `T01B3C83PMK`, refresh the Slack client, and retry in `#oresoftware`. The app needs the `commands` scope for the commands to be installed. If a command appears but Slack reports a dispatch or timeout error, verify the public TLS endpoint, ingress, deployment readiness, and the three-second acknowledgement path. If the app responds that the channel or user is unauthorized, update the reviewed channel registry rather than bypassing policy.

## Architecture

```text
Slack /ores-*, /x-*, or /my-*
  -> one of two canonical provider ingress URLs
  -> fiducia-slack-command
  -> signed-request and installed-app identity checks
  -> channel/user/repository/write-policy authorization
  -> bounded Slack context capture
  -> single-model ai-agent-bridge workflow
  -> idempotent ai-agent-coordinator job
  -> Slack run thread/status
  -> AI Agent Run Queue + owning Linear issue
  -> GitHub branch, draft PR, checks, and final evidence
```

The Slack process is a thin ingress and notification adapter. It does not call model-provider APIs, hold GitHub write credentials, merge pull requests, or implement a second job queue.

## Slack app configuration

All six commands belong to the same reviewed Slack app. The aliases deliberately share the two canonical request URLs; Slack's signed form payload includes the actual `command` value, and the service accepts only the six reviewed names.

| Command | Request URL |
|---|---|
| `/ores-claude` | `https://api.fiducia.cloud/slack/commands/ores-claude` |
| `/x-claude` | `https://api.fiducia.cloud/slack/commands/ores-claude` |
| `/my-claude` | `https://api.fiducia.cloud/slack/commands/ores-claude` |
| `/ores-chatgpt` | `https://api.fiducia.cloud/slack/commands/ores-chatgpt` |
| `/x-chatgpt` | `https://api.fiducia.cloud/slack/commands/ores-chatgpt` |
| `/my-chatgpt` | `https://api.fiducia.cloud/slack/commands/ores-chatgpt` |

Configure the interactivity request URL as:

```text
https://api.fiducia.cloud/slack/interactions
```

Enable escaping of users, channels, and links in command text. Install or reinstall the app only after the public endpoint has TLS, request signing, resource limits, and rollback protection.

Minimum bot scopes for the pilot:

- `commands`;
- `chat:write`;
- `channels:history` for public project channels;
- `groups:history` for approved private project channels;
- `usergroups:read` only when channel bindings authorize Slack user groups.

The bot must be a member of every channel whose recent messages it reads or where it posts run status.

## Channel project registry

`SLACK_PROJECT_REGISTRY_PATH` points to the versioned registry documented in `docs/slack-project-bindings.md`. Every enabled workspace/channel pair must resolve to exactly one Linear project and explicit repository, model, principal, write, retry, token, runtime, concurrency, and spend policies.

The command service never routes by display name. Slack workspace, channel, user, and user-group IDs plus Linear and GitHub stable identifiers are authoritative.

## Context envelope

After authentication and authorization, the service calls `conversations.history` and selects the newest non-bot, non-subtype text messages. It then:

- defaults to five messages;
- caps each message at 4,000 bytes;
- caps the combined context at 32,000 bytes;
- preserves only author ID, timestamp, and bounded text;
- labels the payload `untrusted_channel_context`;
- excludes bot messages and message-event subtypes;
- never treats captured messages as system instructions;
- never logs the prompt or message bodies.

Context depth may be reduced to zero or raised to 10 or 20 only through the reviewed modal choice. Downstream policies may impose a lower limit.

## Durable run identity and fan-out

Each command receives a deterministic `ores-<digest>` run ID. A private create-once journal prevents duplicate Slack deliveries or retries from creating duplicate work.

One accepted live run is represented in five correlated places:

1. Slack acknowledgement and run status/thread;
2. `ai-agent-coordinator.rs` job, using an idempotency key;
3. `ai-agent-bridge.rs` single-model workflow;
4. Linear **AI Agent Run Queue** project plus the owning product issue;
5. GitHub branch, commit, draft pull request, checks, and review evidence when repository writes are allowed.

Linear run queue project ID:

```text
72e891e2-603d-4903-8d08-bd06d204520f
```

The coordinator job is the durable execution authority. Linear and Slack are projections of its lifecycle, not competing queues.

## Environment contract

Secrets remain environment-only:

- `SLACK_SIGNING_SECRET`;
- `SLACK_BOT_TOKEN`;
- `SLACK_BRIDGE_BEARER` when the bridge is non-loopback;
- `SLACK_COORDINATOR_BEARER` when the coordinator is non-loopback.

Reviewed non-secret settings:

- `SLACK_COMMAND_HOST` and `SLACK_COMMAND_PORT`;
- `SLACK_PROJECT_REGISTRY_PATH`;
- `SLACK_COMMAND_STATE_DIR`;
- `SLACK_BRIDGE_URL` and `SLACK_COORDINATOR_URL`;
- `SLACK_CLAUDE_AGENT_KEY` and `SLACK_CHATGPT_AGENT_KEY`;
- `SLACK_LINEAR_RUN_PROJECT_ID`;
- `SLACK_CONTEXT_MESSAGE_COUNT` (`0`, `5`, `10`, or `20`);
- `SLACK_COMMAND_MAX_CONCURRENT_RUNS`;
- `SLACK_COMMAND_DRY_RUN`;
- `SLACK_EXPECTED_APP_ID` and `SLACK_EXPECTED_TEAM_ID` for non-loopback deployment identity enforcement.

The image defaults to port `8151`, context depth `5`, and `SLACK_COMMAND_DRY_RUN=true`.

## Testing the dialog

A malformed `views.open` payload surfaces to a member only as "the dispatch
dialog could not be opened", so the modal is checked at two levels.

**Gating, offline** — `both_provider_modals_respect_slack_block_kit_limits` builds every modal —
both providers x all three write policies x all four context depths, against a
full 100-repository allowlist — and asserts Slack's documented ceilings: modal title ≤ 24 characters,
`private_metadata` ≤ 3000, ≤ 100 blocks, `block_id`/`action_id` ≤ 255,
`plain_text_input.max_length` ≤ 3000, ≤ 100 options per menu, option labels ≤ 75
characters, option values ≤ 150, and every `initial_option` actually present in
its own `options` list. This runs in the normal CI workflow.

**Advisory, browser** — `.github/workflows/block-kit-contract.yml` renders the
real payload in Slack's Block Kit Builder with Playwright and uploads a
screenshot. The fixtures come from `emits_block_kit_fixtures_for_the_browser_contract`, which serialises the output of the same `modal()` the adapter calls, so the browser check cannot drift from what ships.

That job never gates a merge, and it is **inert without credentials**: an
anonymous request to the builder redirects to `app.slack.com/workspace-signin`.
Save a Playwright `storageState` JSON for a workspace that can open the builder
and set it as the `SLACK_BUILDER_STORAGE_STATE` secret to enable it. Until then
the specs skip with an explicit reason and the job summary says so rather than
implying a pass. If the secret is present but the session has expired, the specs
*fail* instead of skipping, so a stale credential cannot hide indefinitely.

## Activation gates

Do not enable live mode until all of these are true:

- the remote Slack app manifest contains all six commands and the interactivity URL;
- app `A0BMBAMM5NJ` is reinstalled to workspace `T01B3C83PMK` after manifest changes;
- Slack signatures and stale/replayed requests fail closed;
- the exact ORESoftware workspace/channel/user IDs are in a reviewed registry;
- the bot has only the required scopes and is a member of the pilot channel;
- the public ingress routes the two canonical command endpoints and the interaction endpoint to a ready `fiducia-slack-command` deployment;
- bridge and coordinator URLs use cluster-local networking or HTTPS plus scoped bearers;
- the coordinator has a worker for `slack_agent_run` jobs;
- Linear run-queue reconciliation is enabled and idempotent;
- repository writes are feature-branch and draft-PR only;
- provider, runtime, retry, concurrency, token, and spend ceilings are active;
- the deployment uses an immutable image digest, External Secrets, NetworkPolicy, health/readiness probes, and a tested rollback;
- a dry-run canary proves command acknowledgement, the blank-command modal, and selected five-message context without exposing message bodies in logs.

## Rollback and incident response

1. Set `SLACK_COMMAND_DRY_RUN=true` or scale the command deployment to zero.
2. Disable the six slash commands or remove their request URLs in Slack.
3. Revoke or rotate the bot token or signing secret if exposure is suspected.
4. Preserve run IDs and metadata-only evidence; do not copy private prompts or channel history into incident tickets.
5. Cancel corresponding coordinator jobs and bridge workflows using their stable IDs.
6. Roll back the Kubernetes image digest, reviewed registry, and Slack manifest independently.
