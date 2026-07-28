# Authoritative lease descriptors for the compatibility API

The legacy bridge API uses a local `lease_id`:

```text
POST /file-leases
POST /file-leases/{lease_id}/renew
POST /file-leases/{lease_id}/release
```

Fiducia does not renew by that local ID. Its authoritative renewal contract
requires the exact canonical repository/path union, owner, current fencing token,
and requested TTL. The bridge therefore persists an immutable descriptor after a
successful external compatibility acquisition and resolves every later mutation
through that descriptor.

## Stored descriptor

Each compatibility acquisition creates one insert-only local UUID and stores:

- descriptor version;
- canonical repository;
- exact canonical path union;
- authenticated/registered `agent_key`;
- current fencing token;
- TTL and authoritative expiry;
- acquisition timestamp;
- active/released status and release timestamp.

Descriptors use the existing durable shared-context mirror under the internal
channel `internal-file-lease-descriptors` and keys prefixed with
`internal.file-lease.v1.`. The standard PostgreSQL restore path hydrates the
channel and context before listeners accept traffic, so a restart does not mint a
new fence or require reacquisition.

Descriptor values contain coordination metadata, not provider credentials or the
Fiducia internal secret. Scoped adapters cannot access generic internal context;
operators must still treat lease ownership metadata as internal operational data.

## Acquisition

For external compatibility acquisition, the bridge:

1. requires a registered agent;
2. rejects recursive requests because the control plane accepts exact path unions;
3. canonicalizes the repository-relative POSIX path and rejects absolute paths,
   backslashes, traversal, empty paths, control characters, and overlong values;
4. sends the exact one-path union to `POST /v1/file-leases/acquire`;
5. requires explicit `acquired=true` and a non-zero fencing token;
6. persists the descriptor before returning the local compatibility ID;
7. releases the authoritative grant best-effort if descriptor persistence fails.

The response includes `compatibility_lease.id`. Clients must retain that ID and
the fencing token.

## Renewal

`POST /file-leases/{lease_id}/renew` accepts only:

```json
{
  "agent_key": "codex-rust",
  "fencing_token": 42,
  "ttl_ms": 30000
}
```

The repository and paths are never accepted from the renewal caller. The bridge
loads them from the descriptor, validates owner and fence locally, and sends the
complete stored union to `POST /v1/file-leases/renew`. A successful response must
report `renewed=true` and preserve the exact fencing token. The descriptor expiry
is updated only after that authoritative success.

Missing, released, expired, malformed, wrong-owner, and stale-token descriptors
fail before a control-plane request. There is no local in-memory renewal fallback
while an external control plane is configured.

## Release and recovery

A successful authoritative release tombstones the descriptor as `released`; later
renewals return not found. The tombstone remains durable for auditability and to
prevent accidental reuse of the local ID.

If the process crashes after the authoritative release but before the tombstone is
persisted, a retry may reach Fiducia and receive its authoritative missing/stale
response. The bridge must never reinterpret that response as a successful local
release.

If the process crashes after authoritative acquisition but before descriptor
persistence, acquisition handling attempts a fenced release and returns an error.
TTL expiry remains the final recovery mechanism if that release cannot complete.

## Safety invariant

A repository writer may continue only while the descriptor is active and the last
renewal of the same exact union and fencing token remains unexpired. Descriptor
loss, expiry, renewal failure, owner mismatch, or token mismatch invalidates write
authority immediately; reacquiring after a gap is a new grant, not renewal.
