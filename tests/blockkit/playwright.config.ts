import { existsSync } from 'node:fs';

import { defineConfig } from '@playwright/test';

// Slack's Block Kit Builder redirects anonymous visitors to /workspace-signin,
// so this suite can only render anything with a saved workspace session.
// Point SLACK_BUILDER_STORAGE_STATE at a Playwright storageState JSON file to
// enable it; without one, every spec skips with an explicit reason rather than
// reporting a green run that checked nothing.
const storageState = process.env.SLACK_BUILDER_STORAGE_STATE;
const authenticated = Boolean(storageState && existsSync(storageState));

// This suite drives a third-party UI (Slack's Block Kit Builder), so it is
// advisory: it runs on a schedule and on demand, never as a merge gate. The
// deterministic ceilings live in the Rust test
// `modal_payload_respects_slack_block_kit_limits`, which does gate.
export default defineConfig({
  testDir: '.',
  timeout: 60_000,
  expect: { timeout: 15_000 },
  retries: 1,
  workers: 1,
  reporter: [['list'], ['html', { open: 'never', outputFolder: 'playwright-report' }]],
  use: {
    headless: true,
    screenshot: 'only-on-failure',
    video: 'off',
    // Slack's builder is heavy; give navigation room without hanging CI.
    navigationTimeout: 45_000,
    ...(authenticated ? { storageState } : {}),
  },
});
