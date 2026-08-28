import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: './specs',
  outputDir: process.env.PLAYWRIGHT_OUTPUT_DIR ?? './test-results',
  timeout: 30_000,
  expect: {
    timeout: 8_000,
  },
  fullyParallel: false,
  forbidOnly: Boolean(process.env.CI),
  retries: process.env.CI ? 1 : 0,
  workers: 1,
  reporter: [['line']],
  use: {
    baseURL:
      process.env.SLACK_BRIDGE_BASE_URL ??
      process.env.SLACK_COMMAND_BASE_URL ??
      process.env.ALEX_MAIN_AGENT_PROBE_URL ??
      'http://127.0.0.1:8160',
    browserName: 'chromium',
    headless: true,
    screenshot: 'only-on-failure',
    trace: 'retain-on-failure',
  },
});
