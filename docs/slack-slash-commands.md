# Slack slash commands: `/my-claude` and `/my-chatgpt`

The dual-model path in [`slack-bridge.md`](./slack-bridge.md) races two fixed
identities from a message prefix. This surface is the complementary one: a
member runs a slash command, picks exactly **one** model plus a target and a
context depth from submenus in a Slack modal, and the resulting task is
broadcast to every configured sink.

Both commands run through the same adapter binary, the same signature
verification, the same allowlists, and the same durable idempotency journal. No
model credentials live here either.

| Command | Dispatches to | Model menu |
|---|---|---|
| `/my-claude` | Claude family only | `SLACK_CLAUDE_MODEL_CHOICES` |
| `/my-chatgpt` | OpenAI family only | `SLACK_OPENAI_MODEL_CHOICES` |

The two menus must not overlap, so a tampered submission cannot make
`/my-claude` dispatch an OpenAI key or vice versa.

## Endpoints

| Endpoint | Slack feature | Purpose |
|---|---|---|
| `POST /slack/commands` | Slash Commands | Opens the dispatch modal |
| `POST /slack/interactions` | Interactivity & Shortcuts | Receives `view_submission` |

Both receive `application/x-www-form-urlencoded` bodies, not JSON. The raw bytes
are signature-verified **before** any parsing, exactly as on `/slack/events`.

## The dialog

`/my-claude` opens a modal with five inputs:

| Block | Control | Notes |
|---|---|---|
| `prompt` | multiline text | Prefilled with any text typed after the command |
| `model` | select | Only the invoked provider's keys |
| `task_type` | select | Ask / Draft new work / Review a repository / Triage a Linear issue |
| `target` | select | From `SLACK_TARGET_CHOICES`; omitted entirely when unset |
| `context_depth` | select | None / 5 / 10 / 25 — **defaults to 5** |

`task_type` only swaps the instruction preamble handed to the agent. It grants
no additional authority, so an unrecognized value degrades to *Ask*.

Slack invalidates `trigger_id` roughly three seconds after issuing it, so
`views.open` is called inline rather than from a spawned task.

## Channel context

By default the last **5** messages in the originating channel are read via
`conversations.history` and appended to the prompt under a
`## Recent channel context` heading, oldest first.

That block is explicitly labeled as background and *not instructions*, so
channel chatter cannot be used to steer the agent by prompt injection. Joins,
leaves, and other subtyped tombstones are dropped. Each message is capped at
1500 bytes and the whole block at 12000 bytes; the composed prompt is then
capped at the adapter's existing `MAX_PROMPT_BYTES`.

**Bot output is excluded entirely** — messages carrying a `bot_id`, messages from
`SLACK_BOT_USER_ID`, and messages with no author. This adapter posts its own
acknowledgements and model replies into the same channel, so including them would
feed the model its own prior output on every later dispatch, and would let any
other integration in the channel (alerting, webhooks, CI bots) plant text
straight into an agent prompt. The Events path already refuses to *act* on bot
messages for this reason; context must not reintroduce them by the back door.

Filtering happens before the depth cut, so the adapter over-fetches (4× the
requested depth, capped at 100) and a chatty channel still yields the full number
of human messages the member asked for.

Set `context_depth` to *No channel context* — or `SLACK_CONTEXT_MESSAGE_DEFAULT=0`
— to disable it. `SLACK_CONTEXT_MESSAGE_MAX` hard-caps what any member can pick.

Reading history requires the bot token to hold `channels:history` (and
`groups:history` for private channels). Without it the dispatch still proceeds,
simply with no context block.

## Broadcast fan-out

One submission fans out to four sinks. The two acknowledgements are posted
*before* the workflow starts, so a slow or failed run still leaves a trace of
what was requested and by whom.

1. **Originating channel** — an acknowledgement, then the model's reply in that
   thread.
2. **Operations channel** — the same acknowledgement re-posted to
   `SLACK_BROADCAST_CHANNEL_ID`, annotated with the source channel. Skipped when
   unset or identical to the origin.
3. **Linear** — an issue in `SLACK_LINEAR_PROJECT_ID` holding the full composed
   prompt, moved `Todo → In Progress → Done` as the run progresses, with the
   model's output added as a comment. Skipped entirely when Linear is unset.
4. **Bridge workflow** — a `single` workflow with `worker_count=1` carrying the
   dispatch, provider, task type, target, and context depth in `meta`.

The workflow response is validated to contain exactly the one requested agent.
Anything else means the bridge routed the work somewhere unintended and is
treated as an error — the dual-model guard would wrongly reject these, so this
path has its own.

## Environment

Everything below is optional; the commands fall back to the dual-model keys and
degrade gracefully when a sink is unconfigured.

| Variable | Default | Purpose |
|---|---|---|
| `SLACK_CLAUDE_COMMAND` | `/my-claude` | Must be one token starting with `/` |
| `SLACK_OPENAI_COMMAND` | `/my-chatgpt` | Must differ from the Claude command |
| `SLACK_CLAUDE_MODEL_CHOICES` | `[SLACK_CLAUDE_AGENT_KEY]` | Ordered CSV menu |
| `SLACK_OPENAI_MODEL_CHOICES` | `[SLACK_OPENAI_AGENT_KEY]` | Ordered CSV menu |
| `SLACK_TARGET_CHOICES` | empty | Ordered CSV of repos/projects |
| `SLACK_CONTEXT_MESSAGE_DEFAULT` | `5` | Preselected context depth |
| `SLACK_CONTEXT_MESSAGE_MAX` | `25` | Hard cap, also the ceiling for the default |
| `SLACK_BROADCAST_CHANNEL_ID` | unset | Operations broadcast channel |
| `SLACK_LINEAR_API_KEY` | unset | Required if `SLACK_LINEAR_TEAM_ID` is set |
| `SLACK_LINEAR_TEAM_ID` | unset | Enables the Linear sink |
| `SLACK_LINEAR_PROJECT_ID` | unset | Agent task project |
| `SLACK_LINEAR_STATE_TODO` | unset | Pending state ID |
| `SLACK_LINEAR_STATE_STARTED` | unset | Running state ID |
| `SLACK_LINEAR_STATE_DONE` | unset | Completed state ID |

Ordered CSV lists reject duplicates rather than silently collapsing them, since
menu order is operator-visible.

## Slack app configuration

1. **Slash Commands** — create `/my-claude` and `/my-chatgpt`, both pointing at
   `POST /slack/commands`.
2. **Interactivity & Shortcuts** — enable it and set the request URL to
   `POST /slack/interactions`.
3. **Scopes** — `commands`, `chat:write`, plus `channels:history` /
   `groups:history` for the context block.
4. Add the bot to every channel in `SLACK_ALLOWED_CHANNEL_IDS` and to the
   broadcast channel.

## Dry run

`SLACK_BRIDGE_DRY_RUN=true` remains the default and applies here too: an
allowlisted command is verified and answered with an ephemeral confirmation of
what *would* have been dispatched, without opening a modal or calling the bridge,
Slack, or Linear.

Follow the same activation sequence as the dual-model path before setting
`SLACK_BRIDGE_DRY_RUN=false`, and add one extra step: confirm the Linear sink
writes into a dedicated agent-task project rather than a delivery project.

## Failure behavior

- Unknown command, non-allowlisted team/channel, or missing `trigger_id` — a
  private ephemeral message; nothing is dispatched.
- Empty/oversized prompt, a model outside the invoked provider's menu, or an
  unconfigured target — an inline modal validation error.
- Bridge at capacity or journal unavailable — an inline modal error; no claim is
  consumed.
- Duplicate submission of the same view — acknowledged and dropped by the
  journal.
- Workflow creation failure — a warning in the thread, and the Linear issue is
  left in the pending state rather than marked running.
