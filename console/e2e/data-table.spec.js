import { readFileSync } from 'node:fs';
import { expect, test } from '@playwright/test';

const fixture = readFileSync(new URL('./fixtures/data-table-sticky.html', import.meta.url), 'utf8');

test('table header stays visible after scrolling 200 rows', async ({ page }, testInfo) => {
  test.skip(testInfo.project.use.viewport.width < 768, 'card fallback hides the table below 768px');

  await page.addInitScript(() => {
    localStorage.setItem('prx-console-token', 'prx_test_token');
  });
  await page.goto('/no-such-page');
  await page.locator('#console-main').evaluate((main, html) => {
    main.insertAdjacentHTML('afterbegin', html);
  }, fixture);
  await page.locator('#sticky-table tbody').evaluate((body) => {
    const fragment = document.createDocumentFragment();
    for (let index = 0; index < 200; index += 1) {
      const row = document.createElement('tr');
      const session = document.createElement('td');
      session.textContent = `session-${index}`;
      const sender = document.createElement('td');
      sender.textContent = 'operator';
      row.append(session, sender);
      fragment.append(row);
    }
    body.append(fragment);
  });

  const positions = await page.locator('#sticky-table').evaluate((scroller) => {
    scroller.scrollTop = scroller.scrollHeight;
    const header = scroller.querySelector('.ui-data-table-head');
    const scrollerBox = scroller.getBoundingClientRect();
    const headerBox = header.getBoundingClientRect();
    return {
      scrollTop: scroller.scrollTop,
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
