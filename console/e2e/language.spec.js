import { expect, test } from '@playwright/test';
import { signIn, stubStatus } from './support.js';

test('a language change persists across reload', async ({ page }) => {
  await signIn(page);
  await stubStatus(page);
  await page.goto('/overview');
  await page.locator('#app-language-select').selectOption('zh');
  await expect.poll(() => page.evaluate(() => document.documentElement.lang)).toBe('zh-CN');
  await expect.poll(() => page.evaluate(() => localStorage.getItem('prx-console-lang'))).toBe('zh');
  await page.reload();
  await expect.poll(() => page.evaluate(() => document.documentElement.lang)).toBe('zh-CN');
  await expect(page.locator('#app-language-select')).toHaveValue('zh');
});
