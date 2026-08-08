import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const workflowPath = ".github/workflows/container-images.yml";
const workflow = readFileSync(workflowPath, "utf8");

function position(label) {
  const index = workflow.indexOf(label);
  assert.notEqual(index, -1, `${workflowPath} is missing ${label}`);
  return index;
}

function step(name) {
  const marker = `      - name: ${name}\n`;
  const start = position(marker);
  const next = workflow.indexOf("\n      - name: ", start + marker.length);
  return workflow.slice(start, next === -1 ? workflow.length : next);
}

test("trusted publication proves the exact pushed digest before writing evidence", () => {
  const publish = position("      - name: Publish digest-addressable image with SBOM and provenance\n");
  const resolve = position("      - name: Resolve exact published image\n");
  const pull = position("      - name: Pull exact published digest\n");
  const inspect = position("      - name: Verify exact published runtime contract\n");
  const scan = position("      - name: Scan exact published digest\n");
  const write = position("      - name: Write machine-readable digest evidence\n");
  const upload = position("      - name: Upload machine-readable digest evidence\n");

  assert.ok(
    publish < resolve &&
      resolve < pull &&
      pull < inspect &&
      inspect < scan &&
      scan < write &&
      write < upload,
    "trusted publication must pull, inspect, and scan the exact digest before evidence upload",
  );

  assert.match(step("Resolve exact published image"), /image_ref="\$\{IMAGE\}@\$\{DIGEST\}"/u);
  assert.match(step("Pull exact published digest"), /docker pull "\$\{PUBLISHED_IMAGE\}"/u);
  assert.match(
    step("Verify exact published runtime contract"),
    /org\.opencontainers\.image\.revision/u,
  );
  assert.match(
    step("Scan exact published digest"),
    /image-ref: \$\{\{ steps\.exact\.outputs\.image_ref \}\}/u,
  );
});

test("pull-request validation remains local while push-only steps are fail-closed", () => {
  const localBuild = position("      - name: Build local image for pull-request contract tests\n");
  const localScan = position("      - name: Scan local pull-request candidate\n");
  const login = position("      - name: Log in to GitHub Container Registry\n");
  const publish = position("      - name: Publish digest-addressable image with SBOM and provenance\n");

  assert.ok(localBuild < localScan && localScan < login && login < publish);
  assert.match(step("Scan local pull-request candidate"), /image-ref: local\/\$\{\{ matrix\.image \}\}:\$\{\{ github\.sha \}\}/u);

  for (const name of [
    "Log in to GitHub Container Registry",
    "Publish digest-addressable image with SBOM and provenance",
    "Resolve exact published image",
    "Pull exact published digest",
    "Verify exact published runtime contract",
    "Scan exact published digest",
    "Write machine-readable digest evidence",
    "Upload machine-readable digest evidence",
  ]) {
    assert.match(step(name), /if: github\.event_name == 'push'/u, `${name} must remain push-only`);
  }
});

test("machine-readable evidence records exact-artifact verification, not tag inference", () => {
  const evidence = step("Write machine-readable digest evidence");
  assert.match(evidence, /schema_version: 2/u);
  assert.match(evidence, /image_ref: \$image_ref/u);
  assert.match(evidence, /exact_digest_pulled: true/u);
  assert.match(evidence, /exact_runtime_contract_verified: true/u);
  assert.match(evidence, /exact_digest_vulnerability_scan: "passed"/u);
  assert.match(evidence, /\.image_ref == \(\.image \+ "@" \+ \.digest\)/u);

  const exactScan = step("Scan exact published digest");
  assert.doesNotMatch(exactScan, /:(?:main|latest|sha-\$\{\{ github\.sha \}\})\b/u);
  assert.match(exactScan, /@sha256|steps\.exact\.outputs\.image_ref/u);
});
