import { createHmac } from 'node:crypto';

import { expect, test } from '@playwright/test';

const signingSecret =
  process.env.SLACK_SIGNING_SECRET ?? 'browser-test-signing-secret';
const expectedAppId = process.env.SLACK_EXPECTED_APP_ID ?? 'A0BMBAMM5NJ';
const expectedTeamId = process.env.SLACK_EXPECTED_TEAM_ID ?? 'T01B3C83PMK';

function signature(body, timestamp, secret = signingSecret) {
  return `v0=${createHmac('sha256', secret)
    .update(`v0:${timestamp}:${body}`)
    .digest('hex')}`;
}

function commandBody(overrides = {}) {
  const fields = new URLSearchParams({
    api_app_id: expectedAppId,
    team_id: expectedTeamId,
    channel_id: 'C_BROWSER_SECURITY_TEST',
    user_id: 'U_BROWSER_SECURITY_TEST',
    command: '/ores-chatgpt',
    text: 'Investigate DEN-1041 without external writes',
    trigger_id: 'browser-security-trigger',
    ...overrides,
  });
  return fields.toString();
}

async function postCommand(
  page,
  {
    body = commandBody(),
    path = '/slack/commands/ores-chatgpt',
    timestamp = Math.floor(Date.now() / 1000),
    requestSignature = signature(body, timestamp),
    includeSignature = true,
    includeTimestamp = true,
  } = {},
) {
  await page.goto('/readyz');
  return page.evaluate(
    async ({ body, path, timestamp, requestSignature, includeSignature, includeTimestamp }) => {
      const headers = {
        'content-type': 'application/x-www-form-urlencoded',
      };
      if (includeTimestamp) {
        headers['x-slack-request-timestamp'] = String(timestamp);
      }
      if (includeSignature) {
        headers['x-slack-signature'] = requestSignature;
      }
      const response = await fetch(path, {
        method: 'POST',
        headers,
        body,
        credentials: 'omit',
        cache: 'no-store',
      });
      return {
        status: response.status,
        body: await response.text(),
        contentType: response.headers.get('content-type'),
      };
    },
    { body, path, timestamp, requestSignature, includeSignature, includeTimestamp },
  );
}

test.beforeEach(async ({ page }) => {
  await page.route('**/*', async (route) => {
    const hostname = new URL(route.request().url()).hostname;
    if (!['127.0.0.1', 'localhost', '::1', '[::1]'].includes(hostname)) {
      await route.abort('blockedbyclient');
      return;
    }
    await route.continue();
  });
});

test('reports an identity-enforced dry-run readiness boundary', async ({ page }) => {
  const response = await page.goto('/readyz');
  expect(response).not.toBeNull();
  expect(response.status()).toBe(200);
  await expect(page.locator('body')).toContainText('"installed_app_identity_enforced":true');
  await expect(page.locator('body')).toContainText('"dry_run":true');
});

test('rejects missing and forged Slack signatures', async ({ page }) => {
  const missing = await postCommand(page, { includeSignature: false });
  expect(missing.status).toBe(401);
  expect(missing.contentType).toContain('application/json');

  const body = commandBody();
  const timestamp = Math.floor(Date.now() / 1000);
  const forged = await postCommand(page, {
    body,
    timestamp,
    requestSignature: `v0=${'0'.repeat(64)}`,
  });
  expect(forged.status).toBe(401);
  expect(forged.body).not.toContain('DEN-1041');
});

test('rejects missing timestamps and signed requests outside the replay window', async ({
  page,
}) => {
  const missing = await postCommand(page, { includeTimestamp: false });
  expect(missing.status).toBe(401);

  const timestamp = Math.floor(Date.now() / 1000) - 301;
  const body = commandBody();
  const stale = await postCommand(page, {
    body,
    timestamp,
    requestSignature: signature(body, timestamp),
  });
  expect(stale.status).toBe(401);
});

test('rejects a valid signature from the wrong installed app', async ({ page }) => {
  const body = commandBody({
    api_app_id: 'A_ATTACKER',
    text: '<script>globalThis.pwned = true</script>',
  });
  const timestamp = Math.floor(Date.now() / 1000);
  const response = await postCommand(page, {
    body,
    timestamp,
    requestSignature: signature(body, timestamp),
  });

  expect(response.status).toBe(403);
  expect(response.body).not.toContain('<script>');
  expect(await page.evaluate(() => globalThis.pwned)).toBeUndefined();
});

test('rejects endpoint and payload provider confusion before policy resolution', async ({
  page,
}) => {
  const body = commandBody({ command: '/ores-claude' });
  const timestamp = Math.floor(Date.now() / 1000);
  const response = await postCommand(page, {
    body,
    timestamp,
    requestSignature: signature(body, timestamp),
  });
  expect(response.status).toBe(400);
});

test('rejects duplicate keys after percent-decoding normalization', async ({ page }) => {
  const body = [
    `api_app_id=${encodeURIComponent(expectedAppId)}`,
    `team_id=${encodeURIComponent(expectedTeamId)}`,
    `team%5Fid=${encodeURIComponent(expectedTeamId)}`,
    'channel_id=C_BROWSER_SECURITY_TEST',
    'user_id=U_BROWSER_SECURITY_TEST',
    'command=%2Fores-chatgpt',
    'text=bounded+task',
    'trigger_id=browser-security-trigger',
  ].join('&');
  const timestamp = Math.floor(Date.now() / 1000);
  const response = await postCommand(page, {
    body,
    timestamp,
    requestSignature: signature(body, timestamp),
  });
  expect(response.status).toBe(400);
});
