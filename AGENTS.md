# Agent Context — ai-agent-bridge

Rust conversation bus where AI agents (Claude, Codex, Gemini, Kimi, Qwen, …)
chat in topic-routed, 32-member chatrooms over HTTP (REST + SSE, `:8142`) and TCP
(newline-delimited JSON, `:8143`). In-memory by default; optional Postgres behind
`--features postgres`. Protocol reference + a drop-in agent prompt:
[`docs/agents-guide.md`](docs/agents-guide.md).

First-class work coordination (`single`, `sequential`, `competitive`, and
reviewer-backed `consensus`) is documented in
[`docs/workflow-orchestration.md`](docs/workflow-orchestration.md). Provider
adapters remain separate processes: they register capabilities, communicate over
the bridge, and acquire the existing Fiducia-backed fenced file leases before
editing repository paths.

Build/test: `cargo build --release --locked` and `cargo test` (in-memory, no DB
needed). The optional `dd-pg-defs` path dep resolves in the `k8s-cluster`
superproject; for a standalone checkout see the README's Development note.

## Git branch policy

Create a focused feature branch from the current `main` branch for agent work,
push it, and open a draft pull request. Do not commit directly to `main` unless
the operator explicitly requests that exception. Preserve unrelated work and
keep each pull request limited to its declared scope.

## Command safety — STRICT (all agents MUST follow)

Never run destructive or irreversible shell commands. To remove or move files,
**always go through git** so the change is tracked and recoverable.

**Blacklisted — do NOT run:**
- `rm`, `rm -rf`, `rmdir`, `unlink`; raw `mv` of tracked files; truncating a file with `>`;
  `dd`, `mkfs`, `shred`, `find … -delete`, `… | xargs rm`, `rsync --delete`,
  recursive `chmod -R` / `chown -R` on broad paths, fork bombs.
- **`git stash` and all stash mutators** (`stash push`/`pop`/`apply`/`drop`/`clear`) — hidden,
  unauditable state that has repeatedly lost work. Use a WIP commit instead
  (`git stash list`/`show` for read-only audit only).
- **Force-push / history rewrite on shared branches:** `git push --force` / `--force-with-lease`,
  `+<refspec>`, `--mirror`, ref deletes; `git rebase`, `git commit --amend`,
  `git filter-branch`, `git filter-repo`.
- **Mass discards:** `git reset --hard`, any `git clean` except `-n`, `git checkout -f`,
  `git checkout -- .`, `git restore .` / `git restore :/`.

**Whitelisted — path-scoped, prefer these:**
- `git rm <path>` / `git rm --cached <path>` — remove through git (recoverable from history).
- `git mv <src> <dst>` — rename/move through git.
- `git restore <single-file>` / `git restore --staged <single-file>`, `git revert <sha>` — reversible.
- `git add <path>`, `git commit`, read-only git (`status`/`log`/`diff`/`show`).
- `git pull --ff-only` — only when the tree is clean and on the repo-approved branch.
- Editor tools for edits. Branch creation (`git switch -c`) is repo-policy-specific — only where
  that repo's AGENTS.md permits it.

If a genuinely destructive action seems unavoidable, **STOP and ask the operator
first** — do not improvise around this rule.
