import { expect, test } from '@playwright/test';
import { fulfillJson, signIn, stubStatus } from './support.js';

function seedMessages() {
  return Array.from({ length: 40 }, (_, index) => ({
    message_id: `m${index}`,
    role: index % 2 === 0 ? 'user' : 'assistant',
    content: `history line ${index} ${'word '.repeat(12)}`
  }));
}

test('a background reply does not pull the reader away from older messages', async ({ page }) => {
  let polls = 0;
  await signIn(page);
  await stubStatus(page);
  await page.route('**/api/sessions/*/messages**', (route) => fulfillJson(route, 200, seedMessages()));
  await page.route('**/api/sessions/*/message', (route) => fulfillJson(route, 202, {
    status: 'accepted',
    job_id: 'job-1',
    work_id: 'work-1',
    poll_url: '/api/jobs/job-1',
    cancel_url: '/api/jobs/job-1/cancel'
  }));
  await page.route(/\/api\/jobs\/job-1$/, (route) => {
    polls += 1;
    if (polls === 1) {
      return fulfillJson(route, 200, { job_id: 'job-1', status: 'running', elapsed_ms: 20, work_id: 'work-1' });
    }
    return fulfillJson(route, 200, {
      job_id: 'job-1',
      status: 'succeeded',
      elapsed_ms: 40,
      work_id: 'work-1',
      result: { reply: 'background reply' }
    });
  });
  await page.goto('/chat/session-1');
  await expect(page.getByText('history line 0')).toBeVisible();
  await page.locator('textarea').fill('new question');
  await page.getByRole('button', { name: 'Send' }).click();
  await expect(page.getByText('Running')).toBeVisible();
  const scroller = page.locator('.ui-chat-scroll');
  await scroller.evaluate((node) => {
    node.scrollTop = 0;
  });
  await expect(page.getByText('background reply')).toBeVisible();
  const scrollTop = await scroller.evaluate((node) => node.scrollTop);
  expect(scrollTop).toBeLessThan(30);
});

test('a media send that is accepted still shows progress when history refresh fails', async ({ page }) => {
  let messageReads = 0;
  await signIn(page);
  await stubStatus(page);
  await page.route('**/api/sessions/*/messages**', (route) => {
    messageReads += 1;
    if (messageReads === 1) {
      return fulfillJson(route, 200, []);
    }
    return fulfillJson(route, 500, { error: 'history unavailable' });
  });
  await page.route('**/api/sessions/*/message', (route) => fulfillJson(route, 202, {
    status: 'accepted',
    job_id: 'job-media',
    work_id: 'work-media',
    poll_url: '/api/jobs/job-media',
    cancel_url: '/api/jobs/job-media/cancel'
  }));
  await page.route(/\/api\/jobs\/job-media$/, (route) => fulfillJson(route, 200, {
    job_id: 'job-media',
    status: 'running',
    elapsed_ms: 20,
    work_id: 'work-media'
  }));
  await page.goto('/chat/session-1');
  await page.locator('input[type="file"]').setInputFiles({
    name: 'note.txt',
    mimeType: 'text/plain',
    buffer: Buffer.from('hello')
  });
  await page.locator('textarea').fill('with a file');
  await page.getByRole('button', { name: 'Send' }).click();
  await expect(page.getByText('Running')).toBeVisible();
  await expect(page.getByText('job-media')).toBeVisible();
  await expect(page.getByText('The send may have reached the gateway.')).toHaveCount(0);
});

test('an uncertain send is not repeated and the draft stays', async ({ page }) => {
  let posts = 0;
  await signIn(page);
  await stubStatus(page);
  await page.route('**/api/sessions/*/messages**', (route) => fulfillJson(route, 200, []));
  await page.route('**/api/sessions/*/message', (route) => {
    posts += 1;
    return route.abort('failed');
  });
  await page.goto('/chat/session-1');
  await page.locator('textarea').fill('keep this draft');
  await page.getByRole('button', { name: 'Send' }).click();
  await expect(page.getByText('The send may have reached the gateway.')).toBeVisible();
  await expect(page.locator('textarea')).toHaveValue('keep this draft');
  expect(posts).toBe(1);
});
