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
3. builds a local `linux/amd64` candidate from the repository-root Docker context;
4. verifies that local candidate uses `nonroot:nonroot`, the expected fixed
   entrypoint, and an empty command;
5. scans the local pull-request candidate for high and critical OS/library
   vulnerabilities with a SHA-pinned Trivy action;
6. on trusted pushes to `main` or a version tag, authenticates only to GHCR and
   publishes the target with full-commit and branch/tag discovery aliases;
7. validates the returned manifest digest and constructs the exact
   `image@sha256:...` reference;
8. pulls that exact published digest back from GHCR;
9. inspects the exact digest for the non-root runtime contract, fixed entrypoint,
   empty command, and source-revision label;
10. scans the exact published digest for high and critical vulnerabilities;
11. attaches BuildKit SBOM and maximum-mode provenance attestations during the
    trusted publication; and
12. only after the exact-digest pull, inspection, and scan succeed, uploads one
    machine-readable JSON evidence artifact per target.

Pull requests build and scan local candidates but never authenticate to or push
into the registry. That lane proves source and Dockerfile behavior before merge;
it is not trusted publication evidence. A trusted push performs a second build,
so the workflow must re-pull, inspect, and scan the exact digest returned by that
publication before recording it as deployable.

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

Each schema-v2 artifact contains the repository, exact 40-character source SHA,
workflow run ID and attempt, Docker target, image name, manifest digest, complete
`image@sha256:...` reference, and explicit evidence that the exact digest was
pulled, runtime-inspected, and vulnerability-scanned successfully. The workflow
requires `image_ref == image + "@" + digest` and validates the source SHA and
digest formats before upload.

GitOps automation and operators must consume this JSON rather than scrape logs
or trust a tag. A digest copied from the publish step before the exact-digest
verification steps finish is not an approved deployment input.

Digest evidence is retained for 90 days in GitHub Actions. The deployment PR must
also copy the selected source SHA, run ID, exact image references, and exact-digest
verification result into its reviewed rollout record so deployment provenance
remains durable after artifact expiration.

## Kubernetes rollout sequence

1. Merge the source PR only after Rust, browser-security, and container candidate
   CI are green.
2. Wait for the trusted `main` publication to pull, inspect, and scan every exact
   published digest successfully.
3. Download all required schema-v2 digest-evidence artifacts from that trusted
   `main` run.
4. Verify every JSON document names the same source SHA and workflow run, and
   records successful exact-digest pull, runtime inspection, and vulnerability
   scan.
5. Open a separate `ORESoftware/k8s-cluster` PR that replaces runtime Git
   clone/build and mutable refs with those exact image digests.
6. Remove `GH_PAT`, source `hostPath`, Cargo build/init containers, and writable
   source volumes from runtime pods.
7. Keep bridge, provider runner, Slack Events ingress, and Slack slash-command
   ingress as independently probed workloads with scoped service accounts,
   resources, NetworkPolicies, and secrets.
8. Render, policy-check, and server-side dry-run every affected overlay.
9. Roll out the bridge first, then a single provider runner, then dry-run Slack
   ingress, and only then enable bounded live Slack dispatch.
10. Record previous digests and prove rollback by digest before scaling.

A source merge does not authorize an automatic cluster rollout. The deployment PR
must contain the reviewed source SHA, workflow run ID, exact image digests,
exact-digest vulnerability result, SBOM/provenance evidence, probe evidence, and
rollback digests.

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
