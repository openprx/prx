import { expect, test } from '@playwright/test';
import { fulfillJson, signIn, stubStatus } from './support.js';

test('table header stays visible after scrolling the jobs table', async ({ page }, testInfo) => {
  test.skip(testInfo.project.use.viewport.width < 768, 'card fallback hides the table below 768px');

  const jobs = Array.from({ length: 220 }, (_, index) => ({
    job_id: `job-${index}`,
    status: 'running',
    label: `gateway:session_message:session-${index}`,
    work_id: `work-${index}`,
    elapsed_ms: index
  }));
  await signIn(page);
  await stubStatus(page);
  await page.route(/\/api\/jobs$/, (route) => fulfillJson(route, 200, { jobs, total: jobs.length }));
  await page.goto('/activity');
  const scroller = page.locator('.ui-data-table-scroll');
  await expect(scroller).toBeVisible();
  const positions = await scroller.evaluate((node) => {
    node.scrollTop = node.scrollHeight;
    const header = node.querySelector('.ui-data-table-head');
    const scrollerBox = node.getBoundingClientRect();
    const headerBox = header.getBoundingClientRect();
    return {
      scrollTop: node.scrollTop,
      headerTop: headerBox.top,
      scrollerTop: scrollerBox.top,
      headerBottom: headerBox.bottom,
      scrollerBottom: scrollerBox.bottom
    };
  });
  expect(positions.scrollTop).toBeGreaterThan(200);
  expect(positions.headerTop).toBeGreaterThanOrEqual(positions.scrollerTop - 1);
  expect(positions.headerBottom).toBeLessThanOrEqual(positions.scrollerBottom + 1);
  expect(Math.abs(positions.headerTop - positions.scrollerTop)).toBeLessThanOrEqual(2);
});
