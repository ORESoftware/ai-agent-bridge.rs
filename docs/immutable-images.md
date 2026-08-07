# Immutable AI-agent bridge images

The bridge, provider runner, Slack Events ingress, and Slack slash-command ingress
are built from one reviewed source tree and one `Cargo.lock`, then published as
separate non-root runtime images:

```text
ghcr.io/oresoftware/fiducia-ai-agent-bridge
ghcr.io/oresoftware/fiducia-ai-agent-runner
ghcr.io/oresoftware/fiducia-slack-bridge
ghcr.io/oresoftware/fiducia-slack-command
```

The Dockerfile has explicit `bridge`, `runner`, `slack`, and `slack-command`
final targets. Each target copies only its release executable and required empty
state directories into a digest-pinned distroless base. No Git client, Cargo
toolchain, source tree, package-manager cache, build credential, or shell is
present in a runtime image.

## CI and publication contract

`.github/workflows/container-images.yml` performs the following for every target:

1. checks out the exact commit without recursively reading the private schema
   submodule;
2. materializes the reviewed credential-free build inputs and verifies the exact
   shared-schema gitlink;
3. builds a local `linux/amd64` image from the repository-root Docker context;
4. verifies `nonroot:nonroot`, the expected fixed entrypoint, and an empty command;
5. scans OS and language packages for high and critical vulnerabilities with a
   SHA-pinned Trivy action;
6. on trusted pushes to `main` or a version tag, authenticates only to GHCR;
7. publishes a full-commit SHA tag plus branch/tag discovery aliases;
8. attaches BuildKit SBOM and maximum-mode provenance attestations;
9. validates the returned `sha256` manifest digest; and
10. uploads one machine-readable JSON evidence artifact per target.

Pull requests build and scan but never authenticate to or push into the registry.
The `main`, version, and full-commit tags are discovery aids only. Kubernetes and
rollback records must use the manifest digest:

```text
ghcr.io/oresoftware/fiducia-ai-agent-bridge@sha256:<digest>
ghcr.io/oresoftware/fiducia-ai-agent-runner@sha256:<digest>
ghcr.io/oresoftware/fiducia-slack-bridge@sha256:<digest>
ghcr.io/oresoftware/fiducia-slack-command@sha256:<digest>
```

## Machine-readable release evidence

A successful trusted publication uploads artifacts named:

```text
image-digest-bridge-<run-id>-<attempt>
image-digest-runner-<run-id>-<attempt>
image-digest-slack-<run-id>-<attempt>
image-digest-slack-command-<run-id>-<attempt>
```

Each artifact contains one JSON document with the repository, exact 40-character
source SHA, workflow run ID and attempt, Docker target, image name, manifest
digest, and complete `image@sha256:...` reference. The workflow validates both the
source SHA and digest formats before upload. GitOps automation and operators must
consume this JSON rather than scrape logs or trust a tag.

Digest evidence is retained for 90 days in GitHub Actions. The deployment PR must
also copy the selected source SHA, run ID, and exact image references into its
reviewed rollout record so deployment provenance remains durable after artifact
expiration.

## Kubernetes rollout sequence

1. Merge the source PR only after Rust, browser-security, and container CI are
   green.
2. Download all required digest-evidence artifacts from the trusted `main` run.
3. Verify every JSON document names the same source SHA and workflow run.
4. Open a separate `ORESoftware/k8s-cluster` PR that replaces runtime Git
   clone/build and mutable refs with those exact image digests.
5. Remove `GH_PAT`, source `hostPath`, Cargo build/init containers, and writable
   source volumes from runtime pods.
6. Keep bridge, provider runner, Slack Events ingress, and Slack slash-command
   ingress as independently probed workloads with scoped service accounts,
   resources, NetworkPolicies, and secrets.
7. Render, policy-check, and server-side dry-run every affected overlay.
8. Roll out the bridge first, then a single provider runner, then dry-run Slack
   ingress, and only then enable bounded live Slack dispatch.
9. Record previous digests and prove rollback by digest before scaling.

A source merge does not authorize an automatic cluster rollout. The deployment PR
must contain the reviewed source SHA, workflow run ID, exact image digests,
vulnerability result, SBOM/provenance evidence, probe evidence, and rollback
digests.

## Secret boundary

Image builds require no provider credential and no GitHub personal access token.
At runtime:

- bridge bearer credentials, scoped adapter credentials, Fiducia internal auth,
  database URLs, provider keys, and Slack credentials come only from approved
  Kubernetes/secret-manager bindings;
- provider configuration may name credential environment variables but never
  contains credential values;
- no secret is passed as a Docker build argument, OCI label, tag, URL,
  command-line argument, workflow record, or image layer;
- pull-request image jobs never receive registry write credentials; and
- private shared-schema/Postgres certification remains in its reviewed central
  GitHub App boundary rather than the credential-free runtime publication lane.

## Completion criteria

DEN-845 is complete only when a follow-up `k8s-cluster` PR deploys the published
digests, removes runtime source/build credentials, establishes independent
readiness contracts, records rollout evidence, and demonstrates controlled
rollback. Live ChatGPT/Claude use additionally requires a successful bounded
provider handoff and authenticated ingress test; green image publication alone is
not proof of a usable production bridge.
