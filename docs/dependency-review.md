# Dependency review ledger

This ledger records narrowly scoped dependency updates that are regenerated from
current `main` rather than merged from stale lockfile branches. It complements,
but does not replace, the repository's exact-head CI, PostgreSQL restart tests,
`cargo audit`, immutable container-image tests, and flags2env contract audit.

| Date | Package | From | To | Scope and evidence |
| --- | --- | --- | --- | --- |
| 2026-07-28 | `tokio-stream` | 0.1.18 | 0.1.19 | Indirect `Cargo.lock` update generated with `cargo update -p tokio-stream --precise 0.1.19`; expected registry checksum `a3d06f0b082ba57c26b79407372e57cf2a1e28124f78e9479fe80322cf53420b`; generator gate proved `Cargo.lock` was the only product file changed before this ledger entry and ran formatting, Clippy with warnings denied, all-target tests, PostgreSQL-feature checks, restart durability, `cargo audit`, and the flags2env contract audit. |

Every entry must describe the exact current-main regeneration command and must
be followed by successful canonical PR and container-image workflows on the
final human-authored head before merge.
