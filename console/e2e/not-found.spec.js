import { expect, test } from '@playwright/test';

test('unknown authenticated path shows not found', async ({ page }) => {
  await page.addInitScript(() => {
    localStorage.setItem('prx-console-token', 'prx_test_token');
  });
  await page.goto('/no-such-page');
  await expect(page.getByRole('heading', { name: 'Not found' })).toBeVisible();
  const overflow = await page.evaluate(
    () => document.documentElement.scrollWidth - document.documentElement.clientWidth
  );
  expect(overflow).toBeLessThanOrEqual(1);
});
