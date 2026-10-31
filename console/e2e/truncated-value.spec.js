import { expect, test } from '@playwright/test';

const revision = '0123456789abcdef'.repeat(4);

test('a clipped revision keeps focus and shows its full text', async ({ page }) => {
  await page.route('**/api/status', (route) => {
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        version: '1.2.3',
        active_config_generation: 1,
        active_config_source_revision: revision,
        desired_config_source_revision: revision,
        config_reload_status: 'idle',
        config_reload_in_progress: false,
        restart_required: [],
        provider_degraded: false
      })
    });
  });
  await page.addInitScript(() => {
    localStorage.setItem('prx-console-token', 'prx_test_token');
  });
  await page.goto('/no-such-page');
  await expect(page.locator('.ui-truncate-id', { hasText: revision }).first()).toBeVisible();

  await page.locator('.ui-skip-link').focus();
  let landed = false;
  for (let step = 0; step < 40 && !landed; step += 1) {
    await page.keyboard.press('Tab');
    landed = await page.evaluate((value) => document.activeElement?.textContent === value, revision);
  }
  expect(landed).toBe(true);

  await expect
    .poll(() =>
      page.evaluate((value) => {
        const active = document.activeElement;
        if (!active || active.textContent !== value) {
          return 'lost';
        }
        const style = getComputedStyle(active);
        if (style.whiteSpace === 'nowrap' || style.overflow === 'hidden') {
          return 'collapsed';
        }
        return 'shown';
      }, revision)
    )
    .toBe('shown');
});
