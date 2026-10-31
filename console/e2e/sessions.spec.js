import { expect, test } from '@playwright/test';
import { fulfillJson, signIn, stubStatus } from './support.js';

async function openSessions(page, sessions = []) {
  await signIn(page);
  await stubStatus(page);
  await page.route('**/api/sessions?**', (route) => fulfillJson(route, 200, sessions));
  await page.route('**/api/sessions', (route) => {
    if (route.request().method() === 'GET') {
      return fulfillJson(route, 200, sessions);
    }
    return route.fallback();
  });
  await page.goto('/sessions');
  await expect(page.getByRole('heading', { name: 'Sessions' })).toBeVisible();
}

test('creates an empty conversation from the shell and from the empty list', async ({ page }) => {
  let posts = 0;
  await openSessions(page, []);
  await page.route('**/api/sessions', async (route) => {
    if (route.request().method() !== 'POST') {
      return fulfillJson(route, 200, []);
    }
    posts += 1;
    expect(route.request().postData()).toBe('{}');
    await page.route('**/api/sessions/*/messages**', (messageRoute) => fulfillJson(messageRoute, 200, []));
    return fulfillJson(route, 201, { session_id: 'session-1', status: 'empty', message_count: 0 });
  });
  await page.locator('.ui-new-conversation-header').click();
  await expect(page).toHaveURL(/\/chat\/session-1$/);
  expect(posts).toBe(1);
});

test('a denied create keeps the list and shows the error', async ({ page }) => {
  await openSessions(page, [{ session_id: 'kept', sender: 'prx_user', channel: 'console', status: 'empty', message_count: 0, last_message_preview: '' }]);
  await page.route('**/api/sessions', (route) => {
    if (route.request().method() === 'POST') {
      return fulfillJson(route, 403, { error: 'denied' });
    }
    return route.fallback();
  });
  await page.locator('.ui-new-conversation-sessions').click();
  await expect(page.locator('#console-main').getByRole('alert').filter({ hasText: 'denied' })).toBeVisible();
  await expect(page.getByText('kept')).toBeVisible();
});

test('an uncertain create is not sent again', async ({ page }) => {
  let posts = 0;
  await openSessions(page, []);
  await page.route('**/api/sessions', (route) => {
    if (route.request().method() === 'POST') {
      posts += 1;
      return route.abort('failed');
    }
    return fulfillJson(route, 200, []);
  });
  await page.locator('.ui-new-conversation-empty').click();
  await expect(page.locator('#new-conversation-uncertain-empty')).toBeVisible();
  await page.getByRole('button', { name: 'Refresh list' }).first().click();
  await expect.poll(() => posts).toBe(1);
});
