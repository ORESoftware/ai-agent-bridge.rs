const REQUIRED_STATEMENT_FRAGMENTS = [
  "row_number() over (partition by m.channel_slug order by m.seq desc)",
  "where m.channel_slug = any($1::text[])",
  "on conflict (agent_key) do update",
  "on conflict (slug) do update",
  "on conflict (channel_slug, seq) do nothing",
  "on conflict (channel_slug, agent_key) do update",
  "where ai_agent_bridge.shared_context.version < excluded.version",
  "updated_at = now()",
];

export class SeaOrmPolicyError extends Error {
  constructor(errors) {
    super(`ai-agent-bridge SeaORM policy failed:\n- ${errors.join("\n- ")}`);
    this.name = "SeaOrmPolicyError";
    this.errors = errors;
  }
}

function require(condition, message, errors) {
  if (!condition) errors.push(message);
}

function escapeRegularExpression(value) {
  // Escape only syntax characters that are special outside a character class.
  // `\-` is an invalid identity escape under Unicode regex mode, so replacing
  // every hyphen broke policy evaluation for `sea-orm` and other Cargo names.
  return value.replace(/[.*+?^${}()|[\]\\]/gu, "\\$&");
}

function dependency(manifest, name) {
  const pattern = new RegExp(`^\\s*${escapeRegularExpression(name)}\\s*=`, "mu");
  return pattern.test(manifest);
}

export function validateSeaOrmPolicy({
  manifest,
  databaseSource,
  restartTest,
  gitmodules,
  sharedContract,
  sharedCommit,
}) {
  const errors = [];
  require(/^[0-9a-f]{40}$/u.test(sharedCommit ?? ""), "shared checkout must be an immutable commit", errors);

  if (typeof manifest !== "string") {
    errors.push("Cargo.toml must be text");
  } else {
    require(dependency(manifest, "sea-orm"), "Cargo.toml must depend on SeaORM", errors);
    require(
      dependency(manifest, "dd-pg-defs-sea-orm"),
      "Cargo.toml must consume dd-pg-defs-sea-orm",
      errors,
    );
    require(!dependency(manifest, "sqlx"), "Cargo.toml must not directly depend on SQLx", errors);
    require(
      !dependency(manifest, "tokio-postgres"),
      "Cargo.toml must not directly depend on tokio-postgres",
      errors,
    );
    require(
      manifest.includes('postgres = ["dep:sea-orm", "dep:dd-pg-defs-sea-orm"]'),
      "the postgres feature must enable only SeaORM persistence dependencies",
      errors,
    );
    require(
      manifest.includes(
        'path = "vendor/k8s-libs-and-shared-defs/pg-defs/generated/rust/sea-orm"',
      ),
      "generated adapter path must remain in the pinned shared submodule",
      errors,
    );
  }

  if (typeof databaseSource !== "string") {
    errors.push("src/db.rs must be text");
  } else {
    for (const forbidden of [
      /\buse\s+sqlx\b/u,
      /\bsqlx::(?:query|query_as|query_scalar|raw_sql|migrate!)/u,
      /\bPgPool(?:Options)?\b/u,
      /\btokio_postgres\b/u,
      /\bMigrator::(?:up|down)\b/u,
      /create\s+(?:schema|table)\b/iu,
    ]) {
      require(!forbidden.test(databaseSource), `src/db.rs contains forbidden path ${forbidden}`, errors);
    }
    for (const required of [
      "DatabaseConnection",
      "ConnectOptions",
      "FromQueryResult",
      "Statement::from_sql_and_values",
      "Value::Array",
      "dd_pg_defs_sea_orm",
      "verify_generated_entity_contract",
      "on conflict (channel_slug, ctx_key) do update",
    ]) {
      require(databaseSource.includes(required), `src/db.rs is missing ${JSON.stringify(required)}`, errors);
    }
    for (const fragment of REQUIRED_STATEMENT_FRAGMENTS) {
      require(
        databaseSource.includes(fragment),
        `src/db.rs lost PostgreSQL semantic fragment ${JSON.stringify(fragment)}`,
        errors,
      );
    }
    require(
      databaseSource.includes("[text_array(channel_slugs)]") &&
        databaseSource.includes("[advisory_key.into()]") === false,
      "channel slug arrays must remain bound SeaORM values",
      errors,
    );
    require(
      databaseSource.includes(".sqlx_logging(false)"),
      "SeaORM SQL logging policy must remain explicit",
      errors,
    );
  }

  if (typeof restartTest !== "string") {
    errors.push("tests/postgres_restart.rs must be text");
  } else {
    require(!/sqlx::|PgPool|raw_sql/u.test(restartTest), "restart test must not use direct SQLx", errors);
    require(
      !/create\s+(?:schema|table)\b/iu.test(restartTest),
      "restart test must not duplicate the canonical schema",
      errors,
    );
    require(
      restartTest.includes("provisioned from canonical pg-defs schema.sql"),
      "restart test must require canonical schema provisioning",
      errors,
    );
    require(
      restartTest.includes("late stale context write is harmless") &&
        restartTest.includes("sequence must resume above the durable high-water") &&
        restartTest.includes("presence must be live, not durable"),
      "restart durability invariants must remain covered",
      errors,
    );
  }

  if (typeof gitmodules !== "string") {
    errors.push(".gitmodules must be text");
  } else {
    require(
      gitmodules.includes('[submodule "vendor/k8s-libs-and-shared-defs"]') &&
        gitmodules.includes("https://github.com/ORESoftware/k8s-libs-and-shared-defs.git"),
      "the canonical shared definitions submodule is missing",
      errors,
    );
  }

  if (sharedContract === null || typeof sharedContract !== "object" || Array.isArray(sharedContract)) {
    errors.push("shared Rust server contract must be an object");
  } else {
    require(
      sharedContract.schemaAuthority?.repository === "ORESoftware/k8s-libs-and-shared-defs" &&
        sharedContract.schemaAuthority?.path === "pg-defs/schema/schema.sql",
      "shared schema authority drifted",
      errors,
    );
    require(sharedContract.rust?.applicationOrm === "SeaORM", "shared contract must require SeaORM", errors);
    require(
      sharedContract.rust?.directSqlxDependency === "forbidden",
      "shared contract must forbid direct SQLx",
      errors,
    );
    require(
      sharedContract.schemaAuthority?.serviceBootMigrations === false,
      "shared contract must forbid boot migrations",
      errors,
    );
    require(sharedContract.migration?.tool === "dpm", "shared migration tool must remain dpm", errors);
    require(
      sharedContract.migration?.repository ===
        "declarative-migrations/declarative-postgres-migrate.rs",
      "shared DPM repository drifted",
      errors,
    );
  }

  if (errors.length > 0) throw new SeaOrmPolicyError(errors);
  return {
    valid: true,
    service: "fiducia-ai-agent-bridge",
    applicationOrm: "SeaORM",
    sharedCommit,
    statementSemantics: REQUIRED_STATEMENT_FRAGMENTS.length,
    directSqlx: false,
    bootMigrations: false,
  };
}
