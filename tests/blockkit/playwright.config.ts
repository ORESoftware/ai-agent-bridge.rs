import { defineConfig } from '@playwright/test';

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
  },
});
