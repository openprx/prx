import { expect, test } from '@playwright/test';
import { fulfillJson, signIn, stubStatus } from './support.js';

async function openTerminals(page) {
  await signIn(page);
  await stubStatus(page);
  await page.route('**/api/chat-sessions/results**', (route) => fulfillJson(route, 200, {
    results: [],
    next_seq: 0,
    oldest_seq: 2
  }));
  await page.route('**/api/chat-sessions', (route) => fulfillJson(route, 200, {
    sessions: [{
      session_id: 'mailbox-1',
      session_ref: 'ref-1',
      label: 'prx_node',
      pid: 9,
      liveness: 'polling',
      queued: 0,
      token: 'hidden-token'
    }]
  }));
  await page.goto('/terminals');
}

test('does not render a session token and reports missed results', async ({ page }) => {
  await openTerminals(page);
  await expect(page.getByText('mailbox-1')).toBeVisible();
  await expect(page.getByText('hidden-token')).toHaveCount(0);
  await expect(page.getByText('Some older results are no longer retained.')).toBeVisible();
  await expect(page.getByText('not the conversations stored by this console')).toBeVisible();
});

test('shows a rate limit and does not retry the assignment', async ({ page }) => {
  let posts = 0;
  await openTerminals(page);
  await page.route('**/api/chat-sessions/*/assign', (route) => {
    posts += 1;
    const body = route.request().postDataJSON();
    expect(body).toEqual({ task: 'inspect logs', disposition: 'queue' });
    expect(body.on_behalf_of).toBeUndefined();
    return fulfillJson(route, 429, { error: 'rate limited' });
  });
  await page.getByLabel('Terminal session').selectOption('mailbox-1');
  await page.getByLabel('Task').fill('inspect logs');
  await page.getByRole('button', { name: 'Assign task' }).click();
  await page.locator('.ui-dialog-confirm').click();
  await expect(page.getByText('The assignment was not retried.')).toBeVisible();
  expect(posts).toBe(1);
});

test('a denied terminal list is distinct from an uncertain assignment', async ({ page }) => {
  await signIn(page);
  await stubStatus(page);
  await page.route('**/api/chat-sessions/results**', (route) => fulfillJson(route, 200, { results: [], next_seq: 0, oldest_seq: 0 }));
  await page.route('**/api/chat-sessions', (route) => fulfillJson(route, 403, { error: 'denied' }));
  await page.goto('/terminals');
  await expect(page.getByText('You do not have permission to view terminal sessions.')).toBeVisible();
});
