#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { validateSeaOrmPolicy } from "./seaorm-policy.mjs";

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const sharedRoot = path.join(repositoryRoot, "vendor", "k8s-libs-and-shared-defs");

function read(relativePath) {
  return readFileSync(path.join(repositoryRoot, relativePath), "utf8");
}

function git(args, cwd) {
  return execFileSync("git", ["-C", cwd, ...args], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
    timeout: 10_000,
    maxBuffer: 64 * 1024,
    env: {
      PATH: process.env.PATH,
      HOME: process.env.HOME,
      LANG: process.env.LANG ?? "C.UTF-8",
      LC_ALL: process.env.LC_ALL ?? "C.UTF-8",
      GIT_CONFIG_GLOBAL: "/dev/null",
      GIT_CONFIG_NOSYSTEM: "1",
      GIT_TERMINAL_PROMPT: "0",
    },
  }).trim();
}

const sharedCommit = git(["rev-parse", "HEAD"], sharedRoot);
const expected = process.env.EXPECTED_SHARED_COMMIT?.trim();
if (expected && expected !== sharedCommit) {
  throw new Error(`shared checkout mismatch: expected ${expected}, received ${sharedCommit}`);
}

const summary = validateSeaOrmPolicy({
  manifest: read("Cargo.toml"),
  databaseSource: read("src/db.rs"),
  restartTest: read("tests/postgres_restart.rs"),
  gitmodules: read(".gitmodules"),
  sharedContract: JSON.parse(
    readFileSync(path.join(sharedRoot, "pg-defs", "rust-server-contract.json"), "utf8"),
  ),
  sharedCommit,
});

console.log(JSON.stringify(summary));
