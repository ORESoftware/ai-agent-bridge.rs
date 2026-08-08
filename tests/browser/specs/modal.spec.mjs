import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { expect, test } from '@playwright/test';

const FIXTURES = join(dirname(fileURLToPath(import.meta.url)), '..', 'fixtures');

function loadView(name) {
  return JSON.parse(readFileSync(join(FIXTURES, name), 'utf8'));
}

const RENDER = `
  window.renderView = (view) => {
    const root = document.getElementById('view');
    document.getElementById('title').textContent = view.title.text;
    document.getElementById('submit').textContent = view.submit.text;
    document.getElementById('close').textContent = view.close.text;
    for (const block of view.blocks) {
      if (block.type !== 'input') throw new Error('unsupported block: ' + block.type);
      const wrapper = document.createElement('div');
      wrapper.dataset.blockId = block.block_id;
      wrapper.dataset.optional = String(Boolean(block.optional));
      const label = document.createElement('label');
      label.textContent = block.label.text;
      label.setAttribute('for', block.element.action_id);
      wrapper.appendChild(label);
      const element = block.element;
      if (element.type === 'static_select') {
        const select = document.createElement('select');
        select.id = element.action_id;
        select.dataset.testid = element.action_id;
        for (const option of element.options) {
          const item = document.createElement('option');
          item.value = option.value;
          item.textContent = option.text.text;
          select.appendChild(item);
        }
        if (element.initial_option) select.value = element.initial_option.value;
        wrapper.appendChild(select);
      } else if (element.type === 'plain_text_input') {
        const input = document.createElement(element.multiline ? 'textarea' : 'input');
        input.id = element.action_id;
        input.dataset.testid = element.action_id;
        if (element.placeholder) input.placeholder = element.placeholder.text;
        if (element.max_length) input.maxLength = element.max_length;
        wrapper.appendChild(input);
      } else {
        throw new Error('unsupported element: ' + element.type);
      }
      root.appendChild(wrapper);
    }
  };
`;

async function open(page, fixture) {
  await page.setContent(`<!doctype html><meta charset="utf-8"><h1 id="title"></h1><div id="view"></div><button id="submit"></button><button id="close"></button><script>${RENDER}<\/script>`);
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  await page.evaluate((view) => window.renderView(view), loadView(fixture));
  expect(errors, 'the modal rendered with browser errors').toEqual([]);
}

const CLAUDE = 'modal.claude.draft-pull-request.json';
const CHATGPT = 'modal.chatgpt.read-only.json';

test('renders every reviewed sub-selection', async ({ page }) => {
  await open(page, CLAUDE);
  for (const field of ['task', 'action', 'repository', 'issue', 'write_scope', 'context_messages']) {
    await expect(page.getByTestId(field), `${field} is missing`).toBeVisible();
  }
});

test('names the provider and the confirming action', async ({ page }) => {
  await open(page, CLAUDE);
  await expect(page.locator('#title')).toHaveText('Run Claude');
  await expect(page.locator('#submit')).toHaveText('Start work');
  await expect(page.locator('#close')).toHaveText('Cancel');
});

test('the ChatGPT surface does not present itself as Claude', async ({ page }) => {
  await open(page, CHATGPT);
  await expect(page.locator('#title')).toHaveText('Run ChatGPT');
});

test('defaults to the last five channel messages', async ({ page }) => {
  await open(page, CLAUDE);
  await expect(page.getByTestId('context_messages')).toHaveValue('5');
  await expect(page.getByTestId('context_messages').locator('option')).toHaveText(['No channel messages', 'Last 5 messages (default)', 'Last 10 messages', 'Last 20 messages']);
});

test('offers the five reviewed actions and defaults to implement', async ({ page }) => {
  await open(page, CLAUDE);
  const actions = page.getByTestId('action');
  await expect(actions).toHaveValue('implement');
  await expect(actions.locator('option')).toHaveText(['Implement and test', 'Investigate and report', 'Review existing work', 'Plan only', 'Triage queue']);
});

test('write scope stops at the channel policy', async ({ page }) => {
  await open(page, CLAUDE);
  await expect(page.getByTestId('write_scope').locator('option')).toHaveText(['Read only', 'Linear issue/comment', 'Feature branch + draft PR']);
});

test('a read-only channel cannot select a write scope at all', async ({ page }) => {
  await open(page, CHATGPT);
  await expect(page.getByTestId('write_scope').locator('option')).toHaveText(['Read only']);
  await expect(page.getByTestId('write_scope')).toHaveValue('read_only');
});

test('only the Linear issue field is optional', async ({ page }) => {
  await open(page, CLAUDE);
  await expect(page.locator('[data-block-id="issue"]')).toHaveAttribute('data-optional', 'true');
  for (const block of ['task', 'action', 'repository', 'write_scope', 'context_messages']) {
    await expect(page.locator(`[data-block-id="${block}"]`)).toHaveAttribute('data-optional', 'false');
  }
});

test('the repository menu offers only allowlisted repositories', async ({ page }) => {
  await open(page, CLAUDE);
  await expect(page.getByTestId('repository').locator('option')).toHaveText(['oresoftware/ai-agent-bridge.rs', 'oresoftware/k8s-cluster']);
  await open(page, CHATGPT);
  await expect(page.getByTestId('repository').locator('option')).toHaveText(['oresoftware/k8s-cluster']);
});

test('the task field is required, multiline, and bounded', async ({ page }) => {
  await open(page, CLAUDE);
  const task = page.getByTestId('task');
  await expect(task).toHaveAttribute('maxlength', '3000');
  await task.fill('repair the deploy gate');
  await expect(task).toHaveValue('repair the deploy gate');
});
