#![forbid(unsafe_code)]

// This crate exists only so credential-free, non-Postgres builds can resolve
// the optional path dependency without reading the private shared-schema repo.
// The canonical generated implementation is certified separately through the
// reviewed GitHub App boundary in ORESoftware/k8s-cluster.
compile_error!(
    "the credential-free dd-pg-defs-sea-orm placeholder was compiled; materialize the exact private shared-schema adapter before enabling the `postgres` feature"
);
