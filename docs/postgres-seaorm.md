# Postgres persistence: SeaORM over the shared declarative schema

The bridge is in-memory first. Building with `--features postgres` adds a
best-effort durable mirror and restart restoration through **SeaORM**.

## Schema and migration ownership

The application does not own DDL or run migrations at startup.

| Concern | Authority |
| --- | --- |
| Canonical shared DDL | `vendor/k8s-libs-and-shared-defs/pg-defs/schema/schema.sql` |
| Generated entities | `vendor/k8s-libs-and-shared-defs/pg-defs/generated/rust/sea-orm` |
| Migration implementation | `declarative-migrations/declarative-postgres-migrate.rs` |
| Reviewed release | `dpm` v0.3.2 |
| Runtime connection, restore, and writes | `src/db.rs` through SeaORM |

The Linux x86-64 release artifact is
`dpm-v0.3.2-x86_64-unknown-linux-gnu.tar.gz` with SHA-256
`4258755a946f6f3a49e33538889523e4736180624a186bddc90180994612d3aa`.

The shared repository is pinned as an immutable Git submodule. Generated
entities are adapters to the DDL contract; they are not migration sources.
Private cross-repository schema certification is performed from
`ORESoftware/k8s-cluster` through its repository-scoped GitHub App/deploy-key
boundary; no PAT or copied private key belongs in this repository.

## Why several queries remain explicit Statements

Ordinary application persistence is SeaORM. Some bridge operations deliberately
use parameterized `sea_orm::Statement` because changing the SQL shape would
change behavior:

- bounded per-channel restore uses a window function;
- channel batches use PostgreSQL text arrays;
- message insert is idempotent on `(channel_slug, seq)`;
- member and channel upserts use `EXCLUDED` expressions;
- timestamps use the database server clock;
- shared context rejects stale writes with an optimistic version guard;
- channel IDs are resolved inside the same write statement.

Values remain bound separately from SQL. No caller value is interpolated.

## Restart guarantees

The database mirror preserves:

- agent metadata;
- active channel metadata and embeddings;
- bounded recent message history;
- per-channel message count and sequence high-water;
- latest versioned shared context.

Live channel membership is intentionally not restored. Agents must rejoin after
restart so stale presence cannot appear active. A delayed context write cannot
overwrite a newer version.

## DPM workflow

Review schema changes in the shared repository:

```sh
cd vendor/k8s-libs-and-shared-defs/pg-defs
scripts/dpm.sh diff
scripts/dpm.sh verify
scripts/dpm.sh review
# scripts/dpm.sh apply  # explicit human action only
```

At the CLI boundary, DPM accepts SQL sources, target and shadow databases:

```sh
dpm apply \
  --source-sql pg-defs/schema/schema.sql \
  --target "$DATABASE_URL" \
  --shadow "$SHADOW_DATABASE_URL" \
  --yes
```

Destructive changes require both reviewed DPM consent flags. Neither DPM nor an
ORM migration command belongs in bridge startup or deployment arguments.

## Local and CI verification

Initialize the reviewed dependencies, then run:

```sh
node --test scripts/seaorm-policy.test.mjs
EXPECTED_SHARED_COMMIT=3c84cab532b27d328378f09fba5841f02644ae3b \
  node scripts/check-seaorm-policy.mjs
cargo clippy --all-targets --locked --features postgres -- -D warnings
cargo test --all-targets --locked
cargo check --all-targets --locked --features postgres
```

The ignored restart test requires a disposable database provisioned from DPM
bootstrap output for the canonical schema:

```sh
export FIDUCIA_BRIDGE_TEST_DATABASE_URL=postgresql://...
cargo test --locked --features postgres --test postgres_restart -- --ignored
```

A successful migration PR must also be grep-clean for direct SQLx pool/query or
boot-migration calls. The SeaORM feature string `sqlx-postgres` is the expected
transport backend and is not a direct SQLx application dependency.

## UI boundary

Maud + HTMX, Leptos, and Dioxus are page-level rendering choices over this same
repository and schema. A Leptos analytics page or Dioxus activity page must
reuse the bridge's SeaORM/auth/owner-scope boundary rather than introduce a
second SQLx store.
