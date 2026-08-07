#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "${repo_root}"

flags_path="vendor/flags-2-env"
schema_path="vendor/k8s-libs-and-shared-defs"
schema_crate_path="${schema_path}/pg-defs/generated/rust/sea-orm"
fixture_path="scripts/ci/fixtures/dd-pg-defs-sea-orm"
expected_shared_commit="${EXPECTED_SHARED_COMMIT:-c8bdc06d74746acc6439f9527ebd02697fdf028b}"

# The public CLI generator is needed by ordinary CI. Fetch only that reviewed
# submodule; never recursively request the private shared-schema repository.
git submodule sync -- "${flags_path}"
git -c protocol.version=2 submodule update --init --depth 1 -- "${flags_path}"

# A real shared-schema checkout must never be replaced or mixed with the
# placeholder. Private Postgres certification owns that checkout separately.
if [ -e "${schema_path}/.git" ]; then
  echo "refusing to overlay an existing shared-schema checkout at ${schema_path}" >&2
  exit 1
fi
if [ -d "${schema_path}" ] && [ -n "$(find "${schema_path}" -mindepth 1 -print -quit)" ]; then
  echo "refusing to overlay non-empty shared-schema path ${schema_path}" >&2
  exit 1
fi

pinned_shared_commit="$(git ls-tree HEAD -- "${schema_path}" | awk '$2 == "commit" { print $3 }')"
if [ "${pinned_shared_commit}" != "${expected_shared_commit}" ]; then
  echo "shared-schema gitlink mismatch: expected ${expected_shared_commit}, got ${pinned_shared_commit:-missing}" >&2
  exit 1
fi

install -d -m 0755 "${schema_crate_path}/src"
install -m 0644 "${fixture_path}/Cargo.toml" "${schema_crate_path}/Cargo.toml"
install -m 0644 "${fixture_path}/src/lib.rs" "${schema_crate_path}/src/lib.rs"

# The placeholder must preserve the package identity locked by Cargo while
# remaining unmistakably non-production and fail-closed if compiled.
grep -Fq 'name = "dd-pg-defs-sea-orm"' "${schema_crate_path}/Cargo.toml"
grep -Fq 'version = "0.1.0"' "${schema_crate_path}/Cargo.toml"
grep -Fq 'compile_error!' "${schema_crate_path}/src/lib.rs"

echo "prepared credential-free non-Postgres build inputs at shared pin ${pinned_shared_commit}"
