# ORESoftware Slack agent commands

Tracking issue: `DEN-1041`

The `fiducia-slack-command` process exposes two exact Slack commands:

```text
/ores-claude [task]
/ores-chatgpt [task]
```

A command with text takes the fast path. A command without text exchanges Slack's short-lived `trigger_id` for a modal with these selections:

- task;
- action: implement, investigate, review, plan, or triage;
- repository from the channel's reviewed allowlist;
- optional Linear issue identifier;
- write scope bounded by channel policy;
- recent channel context: 0, 5, 10, or 20 messages.

The default is the five latest non-bot messages in the channel. Slash commands are channel-level; Slack does not invoke custom slash commands inside message threads. Thread-native work should continue to use the existing app-mention or message-shortcut surface.

## Architecture

```text
Slack /ores-claude or /ores-chatgpt
  -> fiducia-slack-command
  -> signed-request and channel/user policy checks
  -> bounded Slack context capture
  -> single-model ai-agent-bridge workflow
  -> idempotent ai-agent-coordinator job
  -> Slack run thread/status
  -> AI Agent Run Queue + owning Linear issue
  -> GitHub branch, draft PR, checks, and final evidence
```

The Slack process is a thin ingress and notification adapter. It does not call model-provider APIs, hold GitHub write credentials, merge pull requests, or implement a second job queue.

## Slack app configuration

Create both commands in the same reviewed Slack app:

| Command | Request URL |
|---|---|
| `/ores-claude` | `https://<public-host>/slack/commands/ores-claude` |
| `/ores-chatgpt` | `https://<public-host>/slack/commands/ores-chatgpt` |

Configure the interactivity request URL as:

```text
https://<public-host>/slack/interactions
```

Enable escaping of users, channels, and links in command text. Install the app only after the public endpoint has TLS, request signing, resource limits, and rollback protection.

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
- `SLACK_COMMAND_DRY_RUN`.

The image defaults to port `8151`, context depth `5`, and `SLACK_COMMAND_DRY_RUN=true`.

## Activation gates

Do not enable live mode until all of these are true:

- Slack signatures and stale/replayed requests fail closed;
- the exact ORESoftware workspace/channel/user IDs are in a reviewed registry;
- the bot has only the required scopes and is a member of the pilot channel;
- bridge and coordinator URLs use cluster-local networking or HTTPS plus scoped bearers;
- the coordinator has a worker for `slack_agent_run` jobs;
- Linear run-queue reconciliation is enabled and idempotent;
- repository writes are feature-branch and draft-PR only;
- provider, runtime, retry, concurrency, token, and spend ceilings are active;
- the deployment uses an immutable image digest, External Secrets, NetworkPolicy, health/readiness probes, and a tested rollback;
- a dry-run canary proves the selected five-message context without exposing message bodies in logs.

## Rollback and incident response

1. Set `SLACK_COMMAND_DRY_RUN=true` or scale the command deployment to zero.
2. Disable the two slash commands or remove their request URLs in Slack.
3. Revoke/rotate the bot token or signing secret if exposure is suspected.
4. Preserve run IDs and metadata-only evidence; do not copy private prompts or channel history into incident tickets.
5. Cancel corresponding coordinator jobs and bridge workflows using their stable IDs.
6. Roll back the Kubernetes image digest and reviewed registry independently.
