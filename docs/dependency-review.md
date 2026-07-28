# Dependency review ledger

This ledger records narrowly scoped dependency updates that are regenerated from
current `main` rather than merged from stale lockfile branches. It complements,
but does not replace, the repository's exact-head CI, PostgreSQL restart tests,
`cargo audit`, immutable container-image tests, and flags2env contract audit.

| Date | Package | From | To | Scope and evidence |
| --- | --- | --- | --- | --- |
| 2026-07-28 | `tokio-stream` | 0.1.18 | 0.1.19 | Indirect `Cargo.lock` update generated with `cargo update -p tokio-stream --precise 0.1.19`; expected registry checksum `a3d06f0b082ba57c26b79407372e57cf2a1e28124f78e9479fe80322cf53420b`; generator gate proved `Cargo.lock` was the only product file changed before this ledger entry and ran formatting, Clippy with warnings denied, all-target tests, PostgreSQL-feature checks, restart durability, `cargo audit`, and the flags2env contract audit. |
| 2026-07-28 | `thiserror` | 2.0.18 | 2.0.19 | Current-main graph generated with `cargo update -p thiserror@2.0.18 --precise 2.0.19`; verified registry checksums for `thiserror` (`09a43598840e33d5b0331f38c5e30d13bb11c11210a4b58f0d9b18a5a5eefcd9`), `thiserror-impl` (`43cbfe0cf76104d42a574802844187e84a305e531ed54455f11fbde0f10541cd`), and its new `syn` 3.0.3 graph node (`53e9bae58849f64dfa4f5d5ae372c8341f7305f82a3868709269343628b659a3`); generator gate proved `Cargo.lock` was the only product file changed before this ledger entry and ran the full Rust, PostgreSQL restart, audit, and flags2env gates. |

Every entry must describe the exact current-main regeneration command and must
be followed by successful canonical PR and container-image workflows on the
final human-authored head before merge.
