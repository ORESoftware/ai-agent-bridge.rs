import assert from "node:assert/strict";
import test from "node:test";

import {
  SeaOrmPolicyError,
  validateSeaOrmPolicy,
} from "./seaorm-policy.mjs";

const sharedCommit = "3c84cab532b27d328378f09fba5841f02644ae3b";
const valid = {
  manifest: `
[dependencies]
sea-orm = { version = "1.1.20" }
dd-pg-defs-sea-orm = { path = "vendor/k8s-libs-and-shared-defs/pg-defs/generated/rust/sea-orm" }
[features]
postgres = ["dep:sea-orm", "dep:dd-pg-defs-sea-orm"]
`,
  databaseSource: `
use sea_orm::{ConnectOptions, DatabaseConnection, FromQueryResult, Statement, Value};
fn verify_generated_entity_contract() { let _ = dd_pg_defs_sea_orm::AgentsEntity; }
fn statements(channel_slugs: &[String]) {
  options.sqlx_logging(false);
  let _ = Statement::from_sql_and_values(DbBackend::Postgres, "sql", [text_array(channel_slugs)]);
  let _ = Value::Array(ArrayType::String, None);
}
const SQL: &str = "row_number() over (partition by m.channel_slug order by m.seq desc)\nwhere m.channel_slug = any($1::text[])\non conflict (agent_key) do update\non conflict (slug) do update\non conflict (channel_slug, seq) do nothing\non conflict (channel_slug, agent_key) do update\nwhere ai_agent_bridge.shared_context.version < excluded.version\nupdated_at = now()\non conflict (channel_slug, ctx_key) do update";
`,
  restartTest: `
#[ignore = "requires FIDUCIA_BRIDGE_TEST_DATABASE_URL provisioned from canonical pg-defs schema.sql"]
fn invariants() {
  let _ = "late stale context write is harmless";
  let _ = "sequence must resume above the durable high-water";
  let _ = "presence must be live, not durable";
}
`,
  gitmodules: `[submodule "vendor/k8s-libs-and-shared-defs"]
path = vendor/k8s-libs-and-shared-defs
url = https://github.com/ORESoftware/k8s-libs-and-shared-defs.git
`,
  sharedContract: {
    schemaAuthority: {
      repository: "ORESoftware/k8s-libs-and-shared-defs",
      path: "pg-defs/schema/schema.sql",
      serviceBootMigrations: false,
    },
    rust: {
      applicationOrm: "SeaORM",
      directSqlxDependency: "forbidden",
    },
    migration: {
      tool: "dpm",
      repository: "declarative-migrations/declarative-postgres-migrate.rs",
    },
  },
  sharedCommit,
};

function clone(value) {
  return structuredClone(value);
}

function expectInvalid(input, pattern) {
  assert.throws(
    () => validateSeaOrmPolicy(input),
    (error) => {
      assert.ok(error instanceof SeaOrmPolicyError);
      assert.match(error.message, pattern);
      return true;
    },
  );
}

test("the valid fixture binds the service to SeaORM and shared schema ownership", () => {
  assert.deepEqual(validateSeaOrmPolicy(valid), {
    valid: true,
    service: "fiducia-ai-agent-bridge",
    applicationOrm: "SeaORM",
    sharedCommit,
    statementSemantics: 8,
    directSqlx: false,
    bootMigrations: false,
  });
});

test("direct SQLx, PgPool, and raw tokio-postgres fail closed", () => {
  const dependency = clone(valid);
  dependency.manifest += '\nsqlx = { version = "0.8" }\n';
  expectInvalid(dependency, /must not directly depend on SQLx/);

  const pool = clone(valid);
  pool.databaseSource += "\nlet pool: PgPool;\n";
  expectInvalid(pool, /forbidden path/);

  const query = clone(valid);
  query.databaseSource += '\nlet _ = sqlx::query("select 1");\n';
  expectInvalid(query, /forbidden path/);

  const raw = clone(valid);
  raw.manifest += '\ntokio-postgres = "0.7"\n';
  expectInvalid(raw, /must not directly depend on tokio-postgres/);
});

test("complex PostgreSQL restore and upsert semantics cannot be simplified away", () => {
  for (const fragment of [
    "row_number() over (partition by m.channel_slug order by m.seq desc)",
    "on conflict (channel_slug, seq) do nothing",
    "where ai_agent_bridge.shared_context.version < excluded.version",
    "updated_at = now()",
  ]) {
    const input = clone(valid);
    input.databaseSource = input.databaseSource.replace(fragment, "removed");
    expectInvalid(input, /lost PostgreSQL semantic fragment/);
  }

  const array = clone(valid);
  array.databaseSource = array.databaseSource.replace(
    "[text_array(channel_slugs)]",
    "[]",
  );
  expectInvalid(array, /channel slug arrays must remain bound/);
});

test("tests cannot duplicate schema DDL or bypass SeaORM", () => {
  const sqlxTest = clone(valid);
  sqlxTest.restartTest += "\nlet _ = sqlx::raw_sql(TEST_SCHEMA);\n";
  expectInvalid(sqlxTest, /restart test must not use direct SQLx/);

  const copiedSchema = clone(valid);
  copiedSchema.restartTest += "\ncreate table ai_agent_bridge.agents(id uuid);\n";
  expectInvalid(copiedSchema, /must not duplicate the canonical schema/);

  const invariant = clone(valid);
  invariant.restartTest = invariant.restartTest.replace(
    "presence must be live, not durable",
    "presence can persist",
  );
  expectInvalid(invariant, /restart durability invariants must remain covered/);
});

test("shared schema, DPM, adapter, and immutable pin requirements cannot drift", () => {
  const mutable = clone(valid);
  mutable.sharedCommit = "main";
  expectInvalid(mutable, /immutable commit/);

  const missingSubmodule = clone(valid);
  missingSubmodule.gitmodules = "";
  expectInvalid(missingSubmodule, /canonical shared definitions submodule is missing/);

  const adapter = clone(valid);
  adapter.manifest = adapter.manifest.replace(
    /dd-pg-defs-sea-orm[^\n]+\n/u,
    "",
  );
  expectInvalid(adapter, /consume dd-pg-defs-sea-orm/);

  const schema = clone(valid);
  schema.sharedContract.schemaAuthority.path = "service/migrations";
  expectInvalid(schema, /shared schema authority drifted/);

  const dpm = clone(valid);
  dpm.sharedContract.migration.repository = "other/migrator";
  expectInvalid(dpm, /shared DPM repository drifted/);
});
