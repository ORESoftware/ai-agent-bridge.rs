# Agent Context — ai-agent-bridge

Rust conversation bus where AI agents (Claude, Codex, …) chat in topic-routed,
32-member chatrooms over HTTP (REST + SSE, `:8142`) and TCP (newline-delimited
JSON, `:8143`). In-memory by default; optional Postgres behind `--features
postgres`. Protocol reference + a drop-in agent prompt: [`docs/agents-guide.md`](docs/agents-guide.md).

Build/test: `cargo build --release --locked` and `cargo test` (in-memory, no DB
needed). The optional `dd-pg-defs` path dep resolves in the `k8s-cluster`
superproject; for a standalone checkout see the README's Development note.

## Command safety — destructive-command policy

Every agent working in this repo MUST follow this. Prefer version-controlled,
reversible operations; never destroy work irrecoverably.

**Blacklisted — do NOT run:**
- `rm`, `rm -f`, `rm -rf`, `rmdir` — never delete with `rm` (`rm -rf` especially).
- `git reset --hard`, `git clean -fd`/`-fdx`, `git checkout -- .`, `git restore .`
  on a dirty tree — they silently discard uncommitted work.
- `git push -f` / `--force` / `--force-with-lease` to shared branches; history
  rewrites (`rebase`, `filter-branch`) on `main`/`dev`.
- `> file`, `truncate`, `dd` on real files, `find … -delete`, `… | xargs rm`,
  recursive `chmod`/`chown`.
- `mv`-ing tracked files (loses history/staging) or moving paths out of the repo.

**Whitelisted — use these instead:**
- `git rm <path>` / `git rm -r <dir>` instead of `rm` — staged + recoverable from history.
- `git mv <src> <dst>` instead of `mv` for tracked files.
- `git restore <path>` / `git restore --staged <path>` instead of `reset --hard`.
- `git stash` to set changes aside; `git revert <sha>` to undo a commit (no rewrite).
- To "remove" untracked files you didn't create, move them to a `.trash/` dir
  rather than delete them.

If a destructive action truly seems necessary, STOP and ask the human first. Do
not delete or overwrite anything you did not create without explicit confirmation.
