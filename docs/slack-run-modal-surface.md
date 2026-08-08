# Slack run modal surface and acceptance contract

The AI Agent Bridge exposes reviewed `/ores-claude`, `/x-claude`, `/my-claude`, `/ores-chatgpt`, `/x-chatgpt`, and `/my-chatgpt` commands. Authorized bare commands open one Slack modal built by the production `modal` function.

This repository verifies two independent failure classes against that same builder.

## Operator-visible browser contract

`preview_run_modal` is a thin public wrapper around the production builder. It does not maintain a second modal layout. `tests/slack_modal_fixture.rs` freezes two representative `views.open` payloads:

- Claude with draft-pull-request scope and two allowlisted repositories;
- ChatGPT with read-only scope and one allowlisted repository.

The Chromium spec renders the committed JSON into a strict, deliberately small Block Kit renderer. Unknown block or element kinds fail rather than disappearing silently. Assertions cover:

- all reviewed sub-selections;
- provider-specific title;
- confirming and cancel actions;
- five-message context default;
- reviewed action choices;
- write scope bounded by channel policy;
- issue-field optionality;
- repository allowlist; and
- bounded required task input.

The Rust fixture test regenerates from the real builder and fails if committed fixtures drift. The workflow then regenerates again and requires a byte-identical Git tree before opening Chromium.

## Slack API acceptance ceilings

A payload may render perfectly in a browser and still be rejected by `views.open`. Unit tests therefore exercise both providers, all write policies, all offered context depths, a full 100-repository allowlist, and large routing metadata against Slack's hard limits:

- modal title, submit, and close labels;
- private metadata bytes;
- blocks per view;
- options per menu;
- block and action identifiers;
- labels and input maximum lengths;
- option labels and values; and
- every `initial_option` being present in its own options list.

A separate assertion keeps the default repository inside the retained 100-option slice.

## CI and source hygiene

`.github/workflows/slack-modal-surface.yml` is credential-free and uses pinned actions, Rust 1.95.0, Node 22, and Playwright 1.55.0 from the existing lockfile. It runs formatting, focused Rust tests, fixture freshness, strict Clippy, and Chromium rendering.

The workflow rejects tracked `node_modules`, Playwright reports, and browser test results. Dependency installation uses `npm ci --ignore-scripts`; generated browser outputs are ephemeral and uploaded only on failure.

No Slack API call, task dispatch, provider invocation, Linear/GitHub write, or cluster mutation occurs in this test lane.
