import { expect, test } from '@playwright/test';
import { fulfillJson, signIn, stubStatus } from './support.js';

async function openApprovals(page, handler) {
  await signIn(page);
  await stubStatus(page);
  await page.route('**/api/sessions/approvals**', handler);
  await page.goto('/approvals');
}

test('lists only the returned pending approval', async ({ page }) => {
  await openApprovals(page, (route) => fulfillJson(route, 200, {
    approvals: [{
      session_id: 'session-1',
      decision_id: '11111111-1111-4111-8111-111111111111',
      tool: 'file_read',
      effect: 'read',
      arguments_redacted: true,
      pending_expires_at: '2026-10-31T00:00:00Z'
    }],
    next_cursor: null
  }));
  await expect(page.getByText('11111111-1111-4111-8111-111111111111')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Open session' })).toBeVisible();
});

test('distinguishes an empty inbox, a disabled plane, and a denial', async ({ page }) => {
  await openApprovals(page, (route) => fulfillJson(route, 200, { approvals: [], next_cursor: null }));
  await expect(page.getByText('No pending approvals.')).toBeVisible();
  await page.unroute('**/api/sessions/approvals**');
  await page.route('**/api/sessions/approvals**', (route) => fulfillJson(route, 409, { error: 'Decision approvals are disabled' }));
  await page.reload();
  await expect(page.getByText('Decision approvals are disabled.')).toBeVisible();
  await page.unroute('**/api/sessions/approvals**');
  await page.route('**/api/sessions/approvals**', (route) => fulfillJson(route, 403, { error: 'denied' }));
  await page.reload();
  await expect(page.getByText('You do not have permission to view approvals.')).toBeVisible();
  await page.unroute('**/api/sessions/approvals**');
  await page.route('**/api/sessions/approvals**', (route) => fulfillJson(route, 503, { error: 'Approval inbox is unavailable' }));
  await page.reload();
  await expect(page.getByText('Could not load approvals.')).toBeVisible();
  await expect(page.getByText('Decision approvals are disabled.')).toHaveCount(0);
});
