# scripts

Helper scripts for running and operating the bridge.

- `with-flags2env.sh` — a thin wrapper around the pinned `flags-2-env` tool
  (`vendor/flags-2-env`). It reads `--flag=value` arguments, converts them into
  the environment variables the service expects (mapping defined in the repo's
  `.cli-flags.toml`), then execs the command after `--` with that environment
  applied. This keeps secrets (auth tokens, database URLs) env-only and out of
  process listings while still allowing convenient CLI-style startup, e.g.
  `scripts/with-flags2env.sh --http-port=8142 -- cargo run --locked`.
