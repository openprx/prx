import { expect, test } from '@playwright/test';

async function expectNoHorizontalScroll(page) {
  const overflow = await page.evaluate(
    () => document.documentElement.scrollWidth - document.documentElement.clientWidth
  );
  expect(overflow).toBeLessThanOrEqual(1);
}

test('signed-out home shows the token field', async ({ page }) => {
  await page.goto('/');
  await expect(page.getByLabel('Access Token')).toBeVisible();
  await expect(page.getByRole('heading', { name: 'Overview' })).toHaveCount(0);
  await expectNoHorizontalScroll(page);
});

test('signed-out sessions path shows the login view', async ({ page }) => {
  await page.goto('/sessions');
  await expect(page.getByLabel('Access Token')).toBeVisible();
  await expect(page.getByRole('heading', { name: 'Overview' })).toHaveCount(0);
  await expectNoHorizontalScroll(page);
});
