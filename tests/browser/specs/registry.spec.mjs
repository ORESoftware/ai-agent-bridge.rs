import { expect, test } from '@playwright/test';

async function submitRoute(page, overrides = {}) {
  await page.goto('/');

  const values = {
    workspace_id: 'T01B3C83PMK',
    channel_id: 'C0BMF6JDSHX',
    user_id: 'U01AZNU2LJ2',
    repository: '',
    linear_issue_identifier: 'DEN-1280',
    ...overrides,
  };

  await page.locator('#workspace-id').fill(values.workspace_id);
  await page.locator('#channel-id').fill(values.channel_id);
  await page.locator('#user-id').fill(values.user_id);
  await page.locator('#repository').fill(values.repository);
  await page.locator('#linear-issue').fill(values.linear_issue_identifier);
  await page.locator('#resolve-button').click();

  const result = page.locator('#result');
  await expect(result).toHaveAttribute('data-status', /success|error/);
  return JSON.parse(await result.textContent());
}

test('serves restrictive browser security headers', async ({ page }) => {
  const response = await page.goto('/');
  expect(response).not.toBeNull();

  const headers = response.headers();
  expect(headers['content-security-policy']).toContain("default-src 'self'");
  expect(headers['content-security-policy']).toContain("frame-ancestors 'none'");
  expect(headers['cross-origin-opener-policy']).toBe('same-origin');
  expect(headers['cross-origin-resource-policy']).toBe('same-origin');
  expect(headers['referrer-policy']).toBe('no-referrer');
  expect(headers['x-content-type-options']).toBe('nosniff');
  expect(headers['cache-control']).toBe('no-store');
});

test('resolves Hypesiege through the production Rust registry', async ({ page }) => {
  const result = await submitRoute(page);

  expect(result).toMatchObject({
    status: 'resolved',
    workspace_id: 'T01B3C83PMK',
    channel_id: 'C0BMF6JDSHX',
    linear_team_key: 'DEN',
    linear_project_id: 'cd247cb1-870b-471e-89f6-9484df19e798',
    repository: 'hypesiege/hypesiege-mcp-server.rs',
    agent_mode: 'both_parallel',
    write_policy: 'draft_pull_request',
    linear_issue_identifier: 'DEN-1280',
  });
});

test('rejects the misspelled Daedalus channel', async ({ page }) => {
  const result = await submitRoute(page, {
    channel_id: 'C0BMB9GSSKY',
    linear_issue_identifier: 'DEN-1279',
  });

  expect(result).toMatchObject({
    status: 'rejected',
    code: 'unmapped_channel',
  });
});

test('rejects an unauthorized Slack principal', async ({ page }) => {
  const result = await submitRoute(page, {
    channel_id: 'C0BL6BEDYFK',
    user_id: 'U_NOT_ALLOWED',
    linear_issue_identifier: 'DEN-1272',
  });

  expect(result).toMatchObject({
    status: 'rejected',
    code: 'unauthorized_principal',
  });
});

test('rejects a repository escape attempt', async ({ page }) => {
  const result = await submitRoute(page, {
    channel_id: 'C0BMBARQ7N2',
    repository: 'ORESoftware/ai-agent-bridge.rs',
    linear_issue_identifier: 'DEN-1285',
  });

  expect(result).toMatchObject({
    status: 'rejected',
    code: 'repository_not_allowed',
  });
});

test('rejects a Linear issue from another team', async ({ page }) => {
  const result = await submitRoute(page, {
    channel_id: 'C0BMD90HED8',
    linear_issue_identifier: 'ABC-123',
  });

  expect(result).toMatchObject({
    status: 'rejected',
    code: 'issue_team_mismatch',
  });
});
