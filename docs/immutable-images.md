# Immutable bridge and runner images

The bridge and provider runner are built from one reviewed source tree and one
`Cargo.lock`, then published as separate non-root runtime images:

```text
ghcr.io/oresoftware/fiducia-ai-agent-bridge
ghcr.io/oresoftware/fiducia-ai-agent-runner
```

The Dockerfile has explicit `bridge` and `runner` final targets. Both targets use
only the corresponding release executable on a digest-pinned distroless base; no
Git client, Cargo toolchain, source tree, package-manager cache, build credential,
or shell is present in either runtime image.

## CI and publication contract

`.github/workflows/container-images.yml` performs the following for both targets:

1. checks out the exact commit and its pinned submodules;
2. builds a local `linux/amd64` image from the repository-root Docker context;
3. verifies the runtime user is `nonroot:nonroot`, the expected entrypoint is
   fixed, and no mutable command override is baked into the image;
4. scans OS and language packages for unfixed high and critical vulnerabilities
   with a SHA-pinned Trivy action;
5. on trusted pushes to `main` or a version tag, authenticates only to GHCR;
6. publishes a full-commit SHA tag plus branch/tag discovery aliases;
7. attaches BuildKit SBOM and maximum-mode provenance attestations;
8. records the immutable image digest in the GitHub Actions job summary.

Pull requests build and scan but never authenticate to or push into the registry.
The `main` and version tags are discovery aids only. Kubernetes and rollback
records must always use the manifest digest:

```text
ghcr.io/oresoftware/fiducia-ai-agent-bridge@sha256:<digest>
ghcr.io/oresoftware/fiducia-ai-agent-runner@sha256:<digest>
```

## Kubernetes rollout sequence

1. Merge the source PR after Rust and container CI are green.
2. Read the two published digests from the trusted `main` workflow run.
3. Open a separate `k8s-cluster` PR that replaces runtime Git clone/build and
   mutable refs with those exact digests.
4. Remove `GH_PAT`, source `hostPath`, Cargo build/init containers, and writable
   source volumes from the runtime pods.
5. Keep bridge and runner as separate containers or Deployments with independent
   probes, resources, service accounts, NetworkPolicies, and secrets.
6. Render and server-side dry-run every overlay.
7. Roll out the bridge first, then one runner replica with one provider.
8. Record the previous digests and prove rollback by digest before scaling.

A source merge does not authorize an automatic cluster rollout. The deployment PR
must contain the exact reviewed source SHA, image digests, vulnerability result,
SBOM/provenance links, probe evidence, and rollback digests.

## Secret boundary

Image builds require no provider credentials and no GitHub personal access token.
At runtime:

- `API_AUTH_BEARER`, scoped adapter credentials, Fiducia internal auth, database
  URLs, and provider API keys come only from the approved secret manager;
- `AI_PROVIDER_CONFIG_JSON` may name credential environment variables but never
  contains the credential values;
- no secret is passed as a Docker build argument, OCI label, tag, URL, command-line
  argument, workflow record, or image layer;
- pull-request image jobs never receive registry write credentials.

## Remaining DEN-172 work

This source PR establishes reproducible runtime targets and trusted publication.
DEN-172 remains open until a follow-up `k8s-cluster` PR deploys the published
digests, adds independent bridge/runner readiness contracts, removes all runtime
source/build credentials, and demonstrates controlled rollout and rollback.
