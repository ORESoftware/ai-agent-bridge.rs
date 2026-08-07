#!/usr/bin/env bash
set -euo pipefail

# Cargo resolves optional path-package metadata even when the `postgres` feature
# is disabled. Credential-free bridge/browser lanes materialize only the exact
# package manifest pinned by shared-schema commit
# 3c84cab532b27d328378f09fba5841f02644ae3b. They do not claim to certify the
# private generated entities or PostgreSQL behavior; that authority lives in
# ORESoftware/k8s-cluster.
readonly package_root="${1:-vendor/k8s-libs-and-shared-defs/pg-defs/generated/rust/sea-orm}"
readonly expected_manifest_sha256="5828899c653197c7a4ca5600a657bfb8eb82f8dec677d726fcfe7aa42a1b593e"

rm -rf "${package_root}"
mkdir -p "${package_root}/src"

cat >"${package_root}/Cargo.toml" <<'EOF'
[package]
name = "dd-pg-defs-sea-orm"
version = "0.1.0"
edition = "2021"

[dependencies]
sea-orm = { version = "1", features = ["macros", "with-uuid", "with-json", "with-chrono"] }
serde = { version = "1", features = ["derive"] }
EOF

printf '%s\n' '#![forbid(unsafe_code)]' >"${package_root}/src/lib.rs"

actual_manifest_sha256="$(sha256sum "${package_root}/Cargo.toml" | awk '{print $1}')"
if [[ "${actual_manifest_sha256}" != "${expected_manifest_sha256}" ]]; then
  printf 'optional SeaORM metadata digest mismatch: expected %s, got %s\n' \
    "${expected_manifest_sha256}" "${actual_manifest_sha256}" >&2
  exit 1
fi
