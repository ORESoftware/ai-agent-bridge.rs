# GitHub Actions workflows

Continuous-integration definitions for the ai-agent-bridge service.

- `ci.yml` — runs on every pull request and on pushes to `main`. It checks out
  the repo with submodules (needed for the pinned `flags-2-env` tool), then
  enforces formatting (`cargo fmt --check`), lint (`cargo clippy` with warnings
  denied), the full test suite, a `--features postgres` compile check, and a
  `flags2env audit` of `.cli-flags.toml`.

This folder exists so the same quality gates run identically in CI and locally.
