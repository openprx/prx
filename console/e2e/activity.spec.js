import { expect, test } from '@playwright/test';
import { fulfillJson, signIn, stubStatus } from './support.js';

test('cancels a job only after confirmation and shows the acknowledgement limit', async ({ page }) => {
  await signIn(page);
  await stubStatus(page);
  await page.route(/\/api\/jobs$/, (route) => fulfillJson(route, 200, {
    jobs: [{ job_id: 'job-1', status: 'running', label: 'gateway:session_message:session-1', work_id: 'work-1', elapsed_ms: 10 }],
    total: 1
  }));
  await page.route(/\/api\/jobs\/job-1$/, (route) => fulfillJson(route, 200, {
    job_id: 'job-1', status: 'running', work_id: 'work-1', http_status: null, result: null, error: null, elapsed_ms: 10
  }));
  await page.route(/\/api\/jobs\/job-1\/cancel$/, (route) => fulfillJson(route, 200, {
    job_id: 'job-1', work_id: 'work-1', targets: ['work-1:requested']
  }));
  await page.goto('/activity');
  await expect(page.locator('.ui-truncate-id:visible', { hasText: 'gateway:session_message:session-1' })).toBeVisible();
  await page.locator('select').last().selectOption('job-1');
  await page.getByRole('button', { name: 'Cancel turn' }).click();
  await page.locator('.ui-dialog-confirm').click();
  await expect(page.getByText('not proof the process has exited')).toBeVisible();
});

test('shows denial, a stale jobs load, and a runtime steer refusal', async ({ page }) => {
  await signIn(page);
  await stubStatus(page);
  await page.route(/\/api\/jobs$/, (route) => route.abort('failed'));
  await page.goto('/activity');
  await expect(page.getByText('The progress check lost contact.')).toBeVisible();
  await page.unroute(/\/api\/jobs$/);
  await page.route(/\/api\/jobs$/, (route) => fulfillJson(route, 403, { error: 'denied' }));
  await page.reload();
  await expect(page.getByText('You do not have permission to view activity.')).toBeVisible();
  await page.unroute(/\/api\/jobs$/);
  await page.route(/\/api\/jobs$/, (route) => fulfillJson(route, 200, { jobs: [], total: 0 }));
  await page.route('**/api/runtime/tasks', (route) => fulfillJson(route, 200, {
    running: [{ id: 'w42', kind: 'task', name: 'prx_node', state: 'running', pid: 4, steerable: false }],
    unreaped: [],
    total: 1
  }));
  await page.route('**/api/runtime/pools', (route) => fulfillJson(route, 200, { pools: [] }));
  await page.route('**/api/runtime/tasks/w42/message', (route) => fulfillJson(route, 409, { error: 'not steerable', outcome: 'not_steerable' }));
  await page.reload();
  await page.getByRole('tab', { name: 'Runtime' }).click();
  await page.locator('textarea').fill('hold');
  await page.getByRole('button', { name: 'Send to task' }).click();
  await page.locator('.ui-dialog-confirm').click();
  await expect(page.getByText('not steerable')).toBeVisible();
});
