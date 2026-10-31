import { expect, test } from '@playwright/test';

const statusBody = {
  version: '1.2.3',
  uptime_seconds: 12,
  model: 'prx-model',
  memory_backend: 'markdown',
  channels: [],
  gateway_port: 8080,
  provider_degraded: false,
  provider_available: ['prx'],
  provider_unavailable: [],
  active_config_generation: 4,
  active_config_source_revision: 'rev-active',
  desired_config_source_revision: 'rev-desired',
  config_reload_status: 'idle',
  config_reload_in_progress: false,
  config_generation_participants: ['gateway'],
  restart_required: ['channels'],
  last_config_reload_failure: null
};

async function expectNoHorizontalScroll(page) {
  const overflow = await page.evaluate(
    () => document.documentElement.scrollWidth - document.documentElement.clientWidth
  );
  expect(overflow).toBeLessThanOrEqual(1);
}

async function signIn(page) {
  await page.addInitScript(() => {
    localStorage.setItem('prx-console-token', 'prx_test_token');
  });
}

function fulfill(route, status, body) {
  return route.fulfill({
    status,
    contentType: 'application/json',
    body: JSON.stringify(body)
  });
}

test('overview renders generation, revisions, restart, and unknown when a field is absent', async ({ page }) => {
  let generationKnown = true;
  await page.route('**/api/status', (route) => {
    const body = generationKnown ? statusBody : { ...statusBody, active_config_generation: undefined };
    const payload = { ...body };
    if (!generationKnown) {
      delete payload.active_config_generation;
    }
    return fulfill(route, 200, payload);
  });
  await signIn(page);
  await page.goto('/overview');
  const main = page.locator('#console-main');
  await expect(main.getByText('Generation', { exact: true })).toBeVisible();
  await expect(main.getByText('4', { exact: true })).toBeVisible();
  await expect(main.getByText('rev-active')).toBeVisible();
  await expect(main.getByText('rev-desired')).toBeVisible();
  await expect(main.getByText('Required: channels')).toBeVisible();
  await expect(main.getByText('Available', { exact: true })).toBeVisible();
  generationKnown = false;
  await page.reload();
  await expect(main.getByText('Unknown').first()).toBeVisible();
  await expectNoHorizontalScroll(page);
});

test('schedules list, run once, and show denied and conflict states', async ({ page }) => {
  let runPosts = 0;
  let runGets = 0;
  await page.route('**/api/status', (route) => fulfill(route, 200, statusBody));
  await page.route('**/api/cron/**', (route) => {
    const url = route.request().url();
    const method = route.request().method();
    if (url.endsWith('/api/cron/jobs') && method === 'GET') {
      return fulfill(route, 200, {
        jobs: [{ id: 'job-1', name: 'digest', command: 'true', enabled: true, schedule: { kind: 'every', every_ms: 60000 } }]
      });
    }
    if (url.endsWith('/api/cron/status')) {
      return fulfill(route, 200, { enabled: true, disabled_state: 'not_configured', jobs: 1, active: 1, paused: 0, terminal: 0, claimed: 0 });
    }
    if (url.endsWith('/run') && method === 'POST') {
      runPosts += 1;
      return fulfill(route, 200, { job_id: 'job-1', status: 'ok', duration_ms: 4, output: 'done' });
    }
    if (url.endsWith('/runs')) {
      runGets += 1;
      return fulfill(route, 200, { runs: [{ id: 'run-1', status: 'ok', started_at: '2026-10-31T00:00:00Z', output: 'done' }] });
    }
    if (url.includes('/api/cron/jobs/job-1')) {
      return fulfill(route, 200, {
        id: 'job-1',
        name: 'digest',
        enabled: true,
        command: 'true',
        schedule: { kind: 'every', every_ms: 60000 },
        events: []
      });
    }
    return fulfill(route, 404, { error: "Job 'missing' not found" });
  });
  await signIn(page);
  await page.goto('/schedules');
  await expect(page.getByRole('button', { name: 'Open digest' })).toBeVisible();
  await expect(page.getByText(/mark-operator stays on the CLI/)).toBeVisible();
  await page.getByRole('button', { name: 'Open digest' }).click();
  await page.getByRole('button', { name: 'Run now' }).click();
  await expect.poll(() => runPosts).toBe(1);
  await expect.poll(() => runGets).toBe(2);
  await page.waitForTimeout(400);
  expect(runPosts).toBe(1);
  expect(runGets).toBe(2);
  await expectNoHorizontalScroll(page);

  await page.unroute('**/api/cron/**');
  await page.route('**/api/cron/**', (route) => fulfill(route, 403, { error: 'Security policy: read-only mode' }));
  await page.goto('/schedules');
  await expect(page.getByText('Security policy: read-only mode')).toBeVisible();

  await page.unroute('**/api/cron/**');
  await page.route('**/api/cron/**', (route) => {
    const url = route.request().url();
    if (url.endsWith('/run')) {
      return fulfill(route, 409, { error: 'The job changed or is already running.' });
    }
    if (url.endsWith('/runs')) {
      return fulfill(route, 200, { runs: [] });
    }
    return fulfill(route, 200, { id: 'job-1', enabled: true, schedule: { kind: 'every', every_ms: 60000 }, events: [] });
  });
  await page.goto('/schedules/job-1');
  await page.getByRole('button', { name: 'Run now' }).click();
  await expect(page.getByText('The job changed or is already running.')).toBeVisible();
});

test('memory, cost, lifecycle, and experiments keep failures distinct', async ({ page }) => {
  await page.route('**/api/status', (route) => fulfill(route, 200, statusBody));
  await page.route('**/api/memory/search', (route) =>
    fulfill(route, 503, { success: false, error: 'memory tools cannot take the API principal' })
  );
  await signIn(page);
  await page.goto('/memory');
  await expect(page.getByText('memory tools cannot take the API principal', { exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: /store|forget|recall/i })).toHaveCount(0);
  await expectNoHorizontalScroll(page);

  await page.route('**/api/cost**', (route) => {
    const url = new URL(route.request().url());
    if (url.searchParams.get('window') === 'month') {
      return fulfill(route, 500, { error: 'workspace cost report could not be encoded' });
    }
    return fulfill(route, 200, {
      scope: 'workspace',
      estimates: true,
      unpriced_usage: 'omitted',
      outstanding_reservation_micro_usd: 15,
      as_of_utc: '2026-10-31T00:00:00Z',
      selected: { cost_usd: 0, requests: 1 }
    });
  });
  await page.goto('/cost');
  await expect(page.locator('dt', { hasText: /^Settled$/ })).toBeVisible();
  await expect(page.getByText('0', { exact: true })).toBeVisible();
  await expect(page.getByText('workspace').first()).toBeVisible();
  await page.getByRole('button', { name: 'Month' }).click();
  await expect(page.getByText('workspace cost report could not be encoded')).toBeVisible();
  await expect(page.locator('dt', { hasText: /^Settled$/ })).toHaveCount(0);

  await page.route('**/api/skills/lifecycle/status', (route) => fulfill(route, 200, { action: 'lifecycle_status', enabled: true }));
  await page.route('**/api/skills/lifecycle/proposals', (route) =>
    fulfill(route, 200, { proposals: [{ proposal_id: 'p1', skill_id: 'skill-a', status: 'open' }] })
  );
  await page.route('**/api/skills/lifecycle/proposals/p1', (route) =>
    fulfill(route, 200, { proposal_id: 'p1', status: 'open' })
  );
  await page.route('**/api/skills/lifecycle/proposals/p1/publish', (route) =>
    fulfill(route, 403, { error: 'explicit approval grant required' })
  );
  await page.route('**/api/skills/lifecycle/rollback', (route) =>
    fulfill(route, 409, { error: 'committed with repair required', committed_repair_required: true })
  );
  await page.goto('/skills/lifecycle');
  await expect(page.getByText(/not live or benefit acceptance/)).toBeVisible();
  await page.getByRole('button', { name: 'Open proposal p1' }).click();
  await page.getByRole('button', { name: 'Publish' }).click();
  await expect(page.getByText('explicit approval grant required')).toBeVisible();
  await page.getByLabel('Skill id').fill('skill-a');
  await page.getByLabel('Revision id').fill('rev-1');
  await page.getByLabel('Expected generation').fill('3');
  await page.getByRole('button', { name: 'Rollback' }).click();
  await expect(page.getByText('Committed with repair required. This is not a rollback.')).toBeVisible();
  await expectNoHorizontalScroll(page);

  await page.route('**/api/skills/lifecycle/h2', (route) =>
    fulfill(route, 503, { success: false, error: 'the gateway cannot mint an H2 invocation' })
  );
  await page.goto('/skills/h2');
  await expect(page.getByLabel('Action')).not.toContainText('study');
  await page.getByRole('button', { name: 'Submit' }).click();
  await expect(page.getByText('the gateway cannot mint an H2 invocation')).toBeVisible();
  await expect(page.getByText(/There is no job to poll/)).toBeVisible();
  await expectNoHorizontalScroll(page);
});

test('process logs stay labeled when the socket is down', async ({ page }) => {
  await page.route('**/api/status', (route) => fulfill(route, 200, statusBody));
  await signIn(page);
  await page.goto('/logs');
  await expect(page.getByText('These are process logs, not the job state machine.')).toBeVisible();
  await expect(page.getByText('The process log socket is not connected. Lines on screen may be stale.')).toBeVisible();
  await expectNoHorizontalScroll(page);
});
