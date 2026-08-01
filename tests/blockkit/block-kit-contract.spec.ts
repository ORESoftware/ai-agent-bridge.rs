import { existsSync, mkdirSync, readFileSync, readdirSync } from 'node:fs';
import { join, resolve } from 'node:path';

import { expect, test } from '@playwright/test';

// The fixtures are written by the Rust test
// `emits_block_kit_fixtures_for_the_browser_contract`, so this check renders the
// exact payload the adapter hands to views.open rather than a copy that drifts.
// Playwright transpiles specs to CommonJS, so __dirname is the portable anchor
// here — import.meta.url is not available.
const fixtureDir = resolve(__dirname, '../../target/blockkit');
const artifactDir = resolve(__dirname, 'artifacts');

const BUILDER = 'https://app.slack.com/block-kit-builder';

// Every label the adapter is expected to render, per command.
const EXPECTED_LABELS = [
  'What should the agent do?',
  'Model',
  'Task type',
  'Target repository or project',
  'Channel context to include',
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
    `no Block Kit fixtures in ${fixtureDir} — run \`cargo test --lib slack_bridge::commands\` first`,
  ).toBeGreaterThan(0);
});

for (const { name, view } of cases) {
  test(`${name} modal renders in Slack's Block Kit Builder`, async ({ page }) => {
    const url = `${BUILDER}#${encodeURIComponent(JSON.stringify(view))}`;
    await page.goto(url, { waitUntil: 'domcontentloaded' });

    // The builder gates some surfaces behind a workspace login. That is an
    // environment fact, not a defect in our payload, so report it as skipped
    // instead of failing and training people to ignore this job.
    const loginWall = page.locator(
      'text=/sign in to|Sign in to your workspace|enter your workspace/i',
    );
    if (await loginWall.first().isVisible().catch(() => false)) {
      await page.screenshot({ path: join(artifactDir, `${name}-login-wall.png`) });
      test.skip(true, 'Block Kit Builder required a workspace login in this environment');
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
