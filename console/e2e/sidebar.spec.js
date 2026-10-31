import { expect, test } from '@playwright/test';

test('tab from the skip link reaches the header', async ({ page }, testInfo) => {
  test.skip(testInfo.project.use.viewport.width !== 390, 'off-canvas sidebar is checked at 390px');

  await page.addInitScript(() => {
    localStorage.setItem('prx-console-token', 'prx_test_token');
  });
  await page.goto('/no-such-page');
  await page.locator('.ui-skip-link').focus();
  await page.keyboard.press('Tab');

  const landed = await page.evaluate(() => {
    const active = document.activeElement;
    const box = active.getBoundingClientRect();
    return {
      inHeader: active.closest('header') !== null,
      x: box.x,
      width: box.width
    };
  });

  expect(landed.inHeader).toBe(true);
  expect(landed.x).toBeGreaterThanOrEqual(0);
  expect(landed.x + landed.width).toBeLessThanOrEqual(390);
});

test('escape closes the sidebar and returns focus to the menu button', async ({ page }, testInfo) => {
  test.skip(testInfo.project.use.viewport.width !== 390, 'off-canvas sidebar is checked at 390px');

  await page.addInitScript(() => {
    localStorage.setItem('prx-console-token', 'prx_test_token');
  });
  await page.goto('/no-such-page');
  const menu = page.getByRole('button', { name: 'Menu' });
  await menu.click();
  await expect(page.locator('#console-sidebar')).toBeVisible();
  await expect
    .poll(() =>
      page.evaluate(() => document.getElementById('console-sidebar')?.contains(document.activeElement) === true)
    )
    .toBe(true);

  await page.keyboard.press('Escape');
  await expect(menu).toBeFocused();
  const sidebar = page.locator('#console-sidebar');
  await expect.poll(() => sidebar.evaluate((element) => element.hasAttribute('inert'))).toBe(true);
  await expect(sidebar).toHaveCSS('visibility', 'hidden');
});

test('tab from the last nav item stays inside the open sidebar', async ({ page }, testInfo) => {
  test.skip(testInfo.project.use.viewport.width >= 1024, 'the sidebar contains focus only below lg');

  await page.addInitScript(() => {
    localStorage.setItem('prx-console-token', 'prx_test_token');
  });
  await page.goto('/no-such-page');
  await page.getByRole('button', { name: 'Menu' }).click();
  const sidebar = page.locator('#console-sidebar');
  await expect(sidebar).toBeVisible();
  const last = sidebar.getByRole('button', { name: 'Logs' });
  await last.focus();
  await page.keyboard.press('Tab');

  const stayed = await page.evaluate(() => {
    const root = document.getElementById('console-sidebar');
    const active = document.activeElement;
    return root?.contains(active) === true && active?.textContent?.trim() !== 'Logs';
  });
  expect(stayed).toBe(true);
});
