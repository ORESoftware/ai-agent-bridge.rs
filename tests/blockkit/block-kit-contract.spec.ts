import { existsSync, mkdirSync, readFileSync, readdirSync } from 'node:fs';
import { join, resolve } from 'node:path';

import { expect, test } from '@playwright/test';

// The fixtures are written by the Rust test
// `emits_block_kit_fixtures_for_the_browser_contract` in
// src/slack_commands_parts/part14.rs, so this check renders the exact payload
// the adapter hands to views.open rather than a copy that drifts.
// Playwright transpiles specs to CommonJS, so __dirname is the portable anchor
// here — import.meta.url is not available.
const fixtureDir = resolve(__dirname, '../../target/blockkit');
const artifactDir = resolve(__dirname, 'artifacts');

const BUILDER = 'https://app.slack.com/block-kit-builder';

// Every label the dispatch modal is expected to render, per command. These
// mirror `modal()` in src/slack_commands_parts/part5.rs.
const EXPECTED_LABELS = [
  'Task',
  'Action',
  'Repository',
  'Linear issue (optional)',
  'Write scope',
  'Recent channel context',
];

function fixtures(): { name: string; view: unknown }[] {
  if (!existsSync(fixtureDir)) return [];
  return readdirSync(fixtureDir)
    .filter((file) => file.endsWith('.json'))
    .map((file) => ({
      name: file.replace(/\.json$/, ''),
      view: JSON.parse(readFileSync(join(fixtureDir, file), 'utf8')),
    }));
}

const cases = fixtures();

test.beforeAll(() => {
  mkdirSync(artifactDir, { recursive: true });
  // A missing fixture means the Rust suite did not run first. Fail loudly
  // rather than silently reporting a green run that checked nothing.
  expect(
    cases.length,
    `no Block Kit fixtures in ${fixtureDir} — run \`cargo test --lib slack_commands::block_kit\` first`,
  ).toBeGreaterThan(0);
});

// Verified 2026-08-01: an anonymous GET of the builder 302s to
// app.slack.com/workspace-signin ("Find your workspace"). There is no public
// unauthenticated render, so without a session this suite cannot check anything.
const storageState = process.env.SLACK_BUILDER_STORAGE_STATE;
const authenticated = Boolean(storageState && existsSync(storageState));

test.skip(
  !authenticated,
  'SLACK_BUILDER_STORAGE_STATE is not set — Slack Block Kit Builder requires a workspace session',
);

for (const { name, view } of cases) {
  test(`${name} modal renders in Slack's Block Kit Builder`, async ({ page }) => {
    const url = `${BUILDER}#${encodeURIComponent(JSON.stringify(view))}`;
    await page.goto(url, { waitUntil: 'domcontentloaded' });

    // A stale or revoked session lands back on the sign-in page. Fail rather
    // than skip here: the secret exists, so it is meant to work, and a silent
    // skip would hide an expired credential indefinitely.
    if (/workspace-signin|\/signin/.test(page.url())) {
      await page.screenshot({ path: join(artifactDir, `${name}-login-wall.png`) });
      throw new Error(
        `SLACK_BUILDER_STORAGE_STATE did not authenticate; landed on ${page.url()}. Refresh the saved session.`,
      );
    }

    await page.waitForTimeout(2_000);
    await page.screenshot({
      path: join(artifactDir, `${name}-preview.png`),
      fullPage: true,
    });

    const body = (await page.locator('body').innerText()).toLowerCase();

    // Slack surfaces payload problems as an inline error in the builder.
    for (const failure of ['invalid blocks', 'is not a valid', 'errors in your json']) {
      expect(body, `builder reported "${failure}" for ${name}`).not.toContain(failure);
    }

    for (const label of EXPECTED_LABELS) {
      expect(body, `${name} preview is missing "${label}"`).toContain(label.toLowerCase());
    }
  });
}
