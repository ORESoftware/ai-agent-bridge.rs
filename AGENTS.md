# Agent Context — ai-agent-bridge

Rust conversation bus where AI agents (Claude, Codex, …) chat in topic-routed,
32-member chatrooms over HTTP (REST + SSE, `:8142`) and TCP (newline-delimited
JSON, `:8143`). In-memory by default; optional Postgres behind `--features
postgres`. Protocol reference + a drop-in agent prompt: [`docs/agents-guide.md`](docs/agents-guide.md).

Build/test: `cargo build --release --locked` and `cargo test` (in-memory, no DB
needed). The optional `dd-pg-defs` path dep resolves in the `k8s-cluster`
superproject; for a standalone checkout see the README's Development note.

## Command safety — STRICT (all agents MUST follow)

Never run destructive or irreversible shell commands. To remove or move files,
**always go through git** so the change is tracked and recoverable.

**Blacklisted — do NOT run:**
- `rm`, `rm -rf`, `rmdir`, `unlink` — never delete via raw `rm`.
- raw `mv` of tracked files; truncating a tracked file with `>`.
- `git reset --hard`, `git clean -fdx`, `git checkout -- .` / `git restore .` mass-discard.
- `git push --force` / history rewrites on shared branches (esp. `main`).
- `dd`, `mkfs`, `shred`, `find … -delete`, recursive `chmod -R`/`chown -R` on broad paths, fork bombs.

**Whitelisted — safe, prefer these:**
- `git rm` / `git rm --cached` — remove files through git (recoverable via history).
- `git mv` — rename/move through git.
- `git restore <path>` (single file), `git revert`, `git stash` — reversible.
- Editing via the editor tools, `git add`, `git commit`, `git switch -c`.

If a genuinely destructive action seems unavoidable, **STOP and ask the operator
first** — do not improvise around this rule.
