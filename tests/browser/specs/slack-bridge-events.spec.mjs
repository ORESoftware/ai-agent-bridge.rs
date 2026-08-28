// Chromium security lane for the `fiducia-slack-bridge` Events API ingress.
//
// The sibling `slack-ingress.spec.mjs` covers the slash-command service. This
// spec drives the event-callback service on the same signed-request contract and
// additionally locks the two boundaries repaired under DEN-1041: the Slack
// request-URL handshake must complete, and an event minted by any application
// other than the reviewed install must be refused.
//
// Every request is issued by a real browser against a loopback-only deployment
// running in dry-run mode. No live Slack, provider, GitHub, or Linear credential
// participates.

import { createHmac } from 'node:crypto';

import { expect, test } from '@playwright/test';

const signingSecret =
  process.env.SLACK_SIGNING_SECRET ?? 'browser-test-signing-secret';
const expectedAppId = process.env.SLACK_EXPECTED_APP_ID ?? 'A0BMBAMM5NJ';
const allowedTeamId = process.env.SLACK_EXPECTED_TEAM_ID ?? 'T01B3C83PMK';
const allowedChannelId =
  process.env.SLACK_BRIDGE_TEST_CHANNEL_ID ?? 'C_BROWSER_BRIDGE_TEST';
const commandPrefix = process.env.SLACK_COMMAND_PREFIX ?? '!ask-both';

function signature(body, timestamp, secret = signingSecret) {
  return `v0=${createHmac('sha256', secret)
    .update(`v0:${timestamp}:${body}`)
    .digest('hex')}`;
}

let eventSequence = 0;

/** A distinct event id per call keeps idempotency claims from colliding. */
function nextEventId(label) {
  eventSequence += 1;
  return `Ev${label}${eventSequence}`;
}

function eventBody(overrides = {}) {
  const { event: eventOverrides, ...envelopeOverrides } = overrides;
  return JSON.stringify({
    type: 'event_callback',
    event_id: nextEventId('BROWSER'),
    team_id: allowedTeamId,
    api_app_id: expectedAppId,
    event: {
      type: 'app_mention',
      channel: allowedChannelId,
      user: 'U_BROWSER_BRIDGE_TEST',
      text: `${commandPrefix} summarize the DEN-1041 activation gates`,
      ts: '1700000000.000100',
      thread_ts: '1700000000.000100',
      ...eventOverrides,
    },
    ...envelopeOverrides,
  });
}

async function postEvent(
  page,
  {
    body = eventBody(),
    path = '/slack/events',
    timestamp = Math.floor(Date.now() / 1000),
    requestSignature = signature(body, timestamp),
    includeSignature = true,
    includeTimestamp = true,
  } = {},
) {
  await page.goto('/readyz');
  return page.evaluate(
    async ({
      body,
      path,
      timestamp,
      requestSignature,
      includeSignature,
      includeTimestamp,
    }) => {
      const headers = { 'content-type': 'application/json' };
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

/** Signs and posts a payload built outside `eventBody`. */
async function postSigned(page, body, options = {}) {
  const timestamp = options.timestamp ?? Math.floor(Date.now() / 1000);
  return postEvent(page, {
    ...options,
    body,
    timestamp,
    requestSignature: signature(body, timestamp),
  });
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
  await expect(page.locator('body')).toContainText('"dry_run":true');
  await expect(page.locator('body')).toContainText(
    '"installed_app_identity_enforced":true',
  );
});

test('blocks non-loopback browser traffic before a network connection', async ({ page }) => {
  let navigationError;
  try {
    await page.goto('https://example.com/');
  } catch (error) {
    navigationError = error;
  }

  expect(String(navigationError)).toContain('ERR_BLOCKED_BY_CLIENT');
});

test('rejects missing and forged Slack signatures', async ({ page }) => {
  const missing = await postEvent(page, { includeSignature: false });
  expect(missing.status).toBe(401);
  expect(missing.contentType).toContain('application/json');

  const forged = await postEvent(page, {
    requestSignature: `v0=${'0'.repeat(64)}`,
  });
  expect(forged.status).toBe(401);
  expect(forged.body).not.toContain('DEN-1041');
});

test('rejects missing timestamps and signed requests outside the replay window', async ({
  page,
}) => {
  const missing = await postEvent(page, { includeTimestamp: false });
  expect(missing.status).toBe(401);

  const stale = await postSigned(page, eventBody(), {
    timestamp: Math.floor(Date.now() / 1000) - 301,
  });
  expect(stale.status).toBe(401);

  const future = await postSigned(page, eventBody(), {
    timestamp: Math.floor(Date.now() / 1000) + 301,
  });
  expect(future.status).toBe(401);
});

// DEN-1041 regression: Slack's request-URL handshake carries only `token`,
// `challenge`, and `type`. Requiring a workspace made Event Subscriptions
// impossible to enable on this endpoint.
test('completes the Slack request-URL handshake and echoes only the challenge', async ({
  page,
}) => {
  const challenge = '3eZbrw1aBm2rZgRNFdxV2595E9CY3gmdALWMmHkvFXO7tYXAYM8P';
  const response = await postSigned(
    page,
    JSON.stringify({
      token: 'Jhj5dZrVaK7ZwHHjRyZWjbDl',
      challenge,
      type: 'url_verification',
    }),
  );

  expect(response.status).toBe(200);
  expect(JSON.parse(response.body)).toEqual({ challenge });
});

test('ignores a request-URL handshake naming an unapproved workspace', async ({ page }) => {
  const response = await postSigned(
    page,
    JSON.stringify({
      token: 'Jhj5dZrVaK7ZwHHjRyZWjbDl',
      challenge: 'unapproved-workspace-challenge',
      type: 'url_verification',
      team_id: 'T_NOT_APPROVED',
    }),
  );

  expect(response.status).toBe(200);
  expect(response.body).not.toContain('unapproved-workspace-challenge');
  expect(JSON.parse(response.body).ignored).toBe(true);
});

// DEN-1041 hardening: a correct signature over an allowlisted workspace is not
// sufficient; the event must originate from the reviewed application install.
test('rejects a correctly signed event from another installed application', async ({ page }) => {
  const response = await postSigned(
    page,
    eventBody({ api_app_id: 'A_ATTACKER_APP' }),
  );

  expect(response.status).toBe(400);
  expect(response.body).not.toContain('accepted');
});

test('rejects a correctly signed event with no application identity', async ({ page }) => {
  const body = JSON.parse(eventBody());
  delete body.api_app_id;
  const response = await postSigned(page, JSON.stringify(body));

  expect(response.status).toBe(400);
  expect(response.body).not.toContain('accepted');
});

test('ignores events from unapproved workspaces and channels', async ({ page }) => {
  const foreignWorkspace = await postSigned(
    page,
    eventBody({ team_id: 'T_NOT_APPROVED' }),
  );
  expect(foreignWorkspace.status).toBe(200);
  expect(JSON.parse(foreignWorkspace.body).ignored).toBe(true);

  const foreignChannel = await postSigned(
    page,
    eventBody({ event: { channel: 'C_NOT_APPROVED' } }),
  );
  expect(foreignChannel.status).toBe(200);
  expect(JSON.parse(foreignChannel.body).ignored).toBe(true);
});

test('ignores bot-authored messages so the adapter cannot loop on itself', async ({ page }) => {
  const response = await postSigned(
    page,
    eventBody({ event: { bot_id: 'B_SELF' } }),
  );

  expect(response.status).toBe(200);
  expect(JSON.parse(response.body).ignored).toBe(true);
});

test('treats hostile channel text as inert data rather than executable markup', async ({
  page,
}) => {
  const response = await postSigned(
    page,
    eventBody({
      event: {
        text: `${commandPrefix} <script>globalThis.pwned = true</script>`,
      },
    }),
  );

  expect(response.status).toBe(200);
  expect(response.contentType).toContain('application/json');
  expect(response.body).not.toContain('<script>');
  expect(await page.evaluate(() => globalThis.pwned)).toBeUndefined();
});

test('claims a delivered event exactly once so Slack retries cannot fan out', async ({
  page,
}) => {
  const body = eventBody();

  const first = await postSigned(page, body);
  expect(first.status).toBe(200);
  expect(JSON.parse(first.body).accepted).toBe(true);

  const retry = await postSigned(page, body);
  expect(retry.status).toBe(200);
  expect(JSON.parse(retry.body).duplicate).toBe(true);
});

test('rejects malformed JSON bodies that carry a valid signature', async ({ page }) => {
  const response = await postSigned(page, '{"type":"event_callback"');

  expect(response.status).toBe(400);
  expect(response.contentType).toContain('application/json');
});

// DEN-2863: idempotency must not be conditional on spare capacity. The lane
// runs at SLACK_MAX_CONCURRENT_WORKFLOWS=1 precisely so this holds at the
// ceiling; Slack retries on 503, so answering a claimed delivery with
// capacity_exceeded would invite another delivery of work already claimed.
test('recognizes a retry as duplicate rather than shedding it at capacity', async ({ page }) => {
  const body = eventBody();

  const first = await postSigned(page, body);
  expect(first.status).toBe(200);
  expect(JSON.parse(first.body).accepted).toBe(true);

  for (let attempt = 0; attempt < 3; attempt += 1) {
    const retry = await postSigned(page, body);
    expect(retry.status).toBe(200);
    expect(JSON.parse(retry.body).duplicate).toBe(true);
  }
});

test('answers a status lookup for a known delivery while saturated', async ({ page }) => {
  const body = eventBody();
  const accepted = await postSigned(page, body);
  expect(JSON.parse(accepted.body).accepted).toBe(true);
  const deliveredId = JSON.parse(body).event_id;

  const lookup = await postSigned(
    page,
    eventBody({ event: { text: `${commandPrefix} status ${deliveredId}` } }),
  );

  expect(lookup.status).toBe(200);
  const payload = JSON.parse(lookup.body);
  expect(payload.event_id).toBe(deliveredId);
  expect(payload.state).not.toBe('unknown');
});

test('reports unknown for a status lookup that names no known delivery', async ({ page }) => {
  const lookup = await postSigned(
    page,
    eventBody({ event: { text: `${commandPrefix} status EvNeverDelivered` } }),
  );

  expect(lookup.status).toBe(200);
  expect(JSON.parse(lookup.body).state).toBe('unknown');
});

test('rejects a status lookup whose target is not a usable identifier', async ({ page }) => {
  const lookup = await postSigned(
    page,
    eventBody({ event: { text: `${commandPrefix} status ../../etc/passwd` } }),
  );

  expect(lookup.status).toBe(400);
});

test('records cancellation intent and reports terminal and unknown runs', async ({ page }) => {
  // Unknown run: reported, never invented.
  const unknown = await postSigned(
    page,
    eventBody({ event: { text: `${commandPrefix} cancel EvNeverDelivered` } }),
  );
  expect(unknown.status).toBe(200);
  expect(JSON.parse(unknown.body).cancel).toBe('unknown');

  // A delivery that already finished is reported, not silently re-marked.
  const body = eventBody();
  const deliveredId = JSON.parse(body).event_id;
  expect(JSON.parse((await postSigned(page, body)).body).accepted).toBe(true);

  await expect
    .poll(async () => {
      const lookup = await postSigned(
        page,
        eventBody({ event: { text: `${commandPrefix} status ${deliveredId}` } }),
      );
      return JSON.parse(lookup.body).state;
    })
    .toBe('completed');

  const terminal = await postSigned(
    page,
    eventBody({ event: { text: `${commandPrefix} cancel ${deliveredId}` } }),
  );
  expect(terminal.status).toBe(200);
  const payload = JSON.parse(terminal.body);
  expect(payload.cancel).toBe('already_terminal');
  expect(payload.state).toBe('completed');
});

test('accepts a single-provider request and refuses an unknown provider', async ({ page }) => {
  // What is under test is whether the flag parses and routes as work, not
  // whether capacity happens to be free. This lane runs at a ceiling of 1, so a
  // genuinely new delivery may legitimately be shed with 503 while the previous
  // one still holds the only permit. Both 200 and 503 mean the command was
  // understood; only 400 means it was refused.
  for (const model of ['claude', 'chatgpt', 'both']) {
    const accepted = await postSigned(
      page,
      eventBody({ event: { text: `${commandPrefix} --model ${model} explain raft` } }),
    );
    expect([200, 503], `--model ${model} should be understood, not refused`).toContain(
      accepted.status,
    );
    if (accepted.status === 200) {
      expect(JSON.parse(accepted.body).accepted).toBe(true);
    }
  }

  // An unknown provider is refused rather than silently defaulted to both.
  const unknown = await postSigned(
    page,
    eventBody({ event: { text: `${commandPrefix} --model gemini explain raft` } }),
  );
  expect(unknown.status).toBe(400);

  // A flag with no task is not a request.
  const bare = await postSigned(
    page,
    eventBody({ event: { text: `${commandPrefix} --model claude` } }),
  );
  expect(bare.status).toBe(400);
});

test('rejects a cancel whose target is not a usable identifier', async ({ page }) => {
  const response = await postSigned(
    page,
    eventBody({ event: { text: `${commandPrefix} cancel ../../etc/passwd` } }),
  );

  expect(response.status).toBe(400);
});

test('exposes scrapeable outcome counters carrying no Slack content', async ({ page }) => {
  const response = await page.goto('/metrics');
  expect(response).not.toBeNull();
  expect(response.status()).toBe(200);

  const body = await response.text();
  expect(body).toContain('# TYPE slack_bridge_requests_total counter');

  // Every series is declared up front, so an outcome that has not happened yet
  // still scrapes as 0 rather than being absent.
  for (const outcome of ['accepted', 'duplicate', 'rejected_signature', 'rejected_app_identity']) {
    expect(body).toMatch(
      new RegExp(`slack_bridge_requests_total\\{outcome="${outcome}"\\} \\d+`),
    );
  }

  // Earlier tests in this file drove signature and app-identity rejections.
  const rejected = Number(
    /slack_bridge_requests_total\{outcome="rejected_signature"\} (\d+)/.exec(body)?.[1] ?? '0',
  );
  expect(rejected).toBeGreaterThan(0);

  // Metadata only: no Slack identifier, channel, or prompt text may be exposed.
  expect(body).not.toContain(allowedChannelId);
  expect(body).not.toContain(allowedTeamId);
  expect(body).not.toContain(expectedAppId);
  expect(body).not.toContain('DEN-1041');
});
