# GitHub Actions workflows

Continuous-integration definitions for the ai-agent-bridge service.

- `ci.yml` — runs on every pull request and on pushes to `main`. It checks out
  the repo with submodules (needed for the pinned `flags-2-env` tool), then
  enforces formatting, lint (`cargo clippy --locked` with warnings denied), the
  full test suite, a `--features postgres` compile check, `cargo audit`, and a
  `flags2env audit` of `.cli-flags.toml`. Rust, the audit tool, actions, and
  Cargo resolution are pinned.

This folder exists so the same quality gates run identically in CI and locally.

## Security baseline

Every executable workflow uses explicit least-privilege permissions, immutable
third-party action or container references, non-persisted checkout credentials,
concurrency control, and a job timeout. The main CI workflow validates this
directory with the digest-pinned actionlint container. Environment mutation is
forbidden unless this README documents a repository-specific platform exception.
