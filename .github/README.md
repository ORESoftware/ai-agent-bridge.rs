<<<<<<< HEAD
# GitHub automation

Repository automation and dependency-update policy for the bridge. Workflows
must use immutable action pins, locked dependency resolution, and the same
in-memory test path described by the root README.
=======
# .github

GitHub Actions for `fiducia-ai-agent-bridge.rs` — CI (fmt, clippy `-D warnings`, locked tests,
`cargo audit`) plus the repo's deploy/docker/flags workflows where present.
Workflow actions are pinned to full commit SHAs per the fleet's
reproducible-build policy (audited by the monorepo's `audit-repo-state.sh`).
>>>>>>> ae15b92 (Adopt fiducia-telemetry; add trace/panic layers; surface shed persist writes)
