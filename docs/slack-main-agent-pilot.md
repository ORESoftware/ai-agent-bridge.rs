# alex-main-agent pilot binding

Tracking issues: `DEN-1041`, `DEN-1298`

This document records the reviewed, non-secret identifiers for the first ORESoftware Slack command pilot. It does not contain the Slack bot token, signing secret, app configuration token, bridge bearer, or coordinator bearer.

## Slack identity

| Resource | Stable identifier |
|---|---|
| App | `alex-main-agent` |
| App ID | `A0BMBAMM5NJ` |
| Workspace/team ID | `T01B3C83PMK` |
| Pilot channel | `#oresoftware` |
| Pilot channel ID | `C0BKP2N3LG7` |
| Pilot operator | Alex Mills |
| Pilot operator user ID | `U01AZNU2LJ2` |

The Slack app is installed in the workspace. The reviewed command and interaction configuration lives in `slack-app/manifest.yaml`. The app is not considered activated until the remote manifest is reconciled with that file, the app is reinstalled after any scope or command change, and the public command service passes a signed dry-run canary.

## Commands and invocation

The canonical commands are:

```text
/ores-claude [task]
/ores-chatgpt [task]
```

The installed app also exposes these convenience aliases:

```text
/x-claude [task]
/x-chatgpt [task]
/my-claude [task]
/my-chatgpt [task]
```

Type a command in the message composer of an authorized project channel. Supplying text dispatches the task directly; leaving the command empty opens the reviewed task modal. Custom slash commands are not invoked from message threads, so start the run in the channel composer and continue in the app's status thread.

## Public request surface

The reviewed manifest exposes only two provider command endpoints plus one interaction endpoint:

```text
https://api.fiducia.cloud/slack/commands/ores-claude
https://api.fiducia.cloud/slack/commands/ores-chatgpt
https://api.fiducia.cloud/slack/interactions
```

`/x-claude` and `/my-claude` share the canonical Claude endpoint. `/x-chatgpt` and `/my-chatgpt` share the canonical ChatGPT endpoint. Slack includes the actual command name in the signed form payload, and the runtime rejects any command outside the six reviewed names.

The application must verify Slack signatures, request freshness, app ID, and workspace ID before parsing or journaling a request. No gateway authentication cookie or operator bearer may be required on these three Slack-signed endpoints.

## Linear routing

| Resource | Stable identifier |
|---|---|
| Linear team | Denman (`DEN`) |
| Linear team ID | `eb8ab169-5afe-4b6f-9cab-3f2aa3e887dc` |
| Owning project | `github.com/ORESoftware` |
| Owning project ID | `7abf8be2-ffa5-4507-bd09-43aa59ca8718` |
| AI Agent Run Queue project ID | `72e891e2-603d-4903-8d08-bd06d204520f` |

The initial repository allowlist is:

```text
ORESoftware/ai-agent-bridge.rs
ORESoftware/ai-agent-coordinator.rs
ORESoftware/k8s-cluster
```

The initial write policy is `draft_pull_request`. Only `U01AZNU2LJ2` is authorized during the pilot. Broader users, channels, repositories, or user groups require a reviewed registry change.

## Applying and verifying the manifest

Slack app manifests replace the app configuration as a whole. Export or review the current remote manifest before applying `slack-app/manifest.yaml`, then validate the complete document.

Using the Slack app settings UI:

1. Open app `A0BMBAMM5NJ`.
2. Open **App Manifest** and export or copy the current remote manifest.
3. Reconcile it with `slack-app/manifest.yaml`; do not discard unrelated reviewed settings.
4. Validate and save the merged manifest.
5. Reinstall the app to workspace `T01B3C83PMK` if Slack reports changed commands, scopes, or features.
6. Refresh the Slack client and type `/ores-` in the `#oresoftware` composer. All six commands should appear in autocomplete.
7. Invoke `/ores-chatgpt` with no text to verify the modal, then run a bounded dry-run task.

Using an app configuration token:

```bash
manifest_json="$(yq -o=json '.' slack-app/manifest.yaml)"

slack api apps.manifest.export \
  --team T01B3C83PMK \
  --token "$SLACK_CONFIG_TOKEN" \
  "$(jq -n --arg app_id A0BMBAMM5NJ '{app_id:$app_id}')" \
  > remote-alex-main-agent-manifest.json

slack api apps.manifest.validate \
  --team T01B3C83PMK \
  --token "$SLACK_CONFIG_TOKEN" \
  "$(jq -n --arg manifest "$manifest_json" '{manifest:$manifest}')"

slack api apps.manifest.update \
  --team T01B3C83PMK \
  --token "$SLACK_CONFIG_TOKEN" \
  "$(jq -n --arg app_id A0BMBAMM5NJ --arg manifest "$manifest_json" '{app_id:$app_id,manifest:$manifest}')"
```

App configuration tokens are short-lived and must remain outside Git, logs, Linear, and Slack messages. The exported remote manifest is non-secret configuration, but request URLs and internal feature settings should still be handled as operational data.

## Visibility and runtime diagnosis

Use the symptom to distinguish the failing layer:

| Symptom | Likely layer |
|---|---|
| No command in autocomplete | Remote manifest was not applied, app was not reinstalled, `commands` scope is absent, or the client needs refresh |
| Command appears, then `dispatch_failed` or timeout | Public TLS/DNS/ingress/deployment is unavailable or acknowledgement exceeded Slack's deadline |
| Blank command does not open a modal | Interactivity URL, `views.open`, bot token, or trigger-ID timing is broken |
| Ephemeral unauthorized response | Workspace, app, channel, user, repository, or write policy does not match the reviewed registry |
| Dry-run succeeds but no real work starts | `SLACK_COMMAND_DRY_RUN` is still true or bridge/coordinator live-dispatch credentials are absent |

## Secret-store and deployment contract

The Kubernetes deployment must source these values from External Secrets:

- `SLACK_BOT_TOKEN`;
- `SLACK_SIGNING_SECRET`;
- `SLACK_BRIDGE_BEARER` when live bridge dispatch is enabled;
- `SLACK_COORDINATOR_BEARER` when live coordinator dispatch is enabled.

The deployment must also set these non-secret identity values:

```text
SLACK_EXPECTED_APP_ID=A0BMBAMM5NJ
SLACK_EXPECTED_TEAM_ID=T01B3C83PMK
```

The first cluster rollout remains `SLACK_COMMAND_DRY_RUN=true`. It may read the five latest approved non-bot messages and post a metadata-only dry-run acknowledgement, but it must not create bridge workflows, coordinator jobs, Linear records, GitHub branches, or pull requests until the live activation gates in `docs/slack-ores-commands.md` are satisfied.
