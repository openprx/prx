import { expect, test } from '@playwright/test';

async function boot(page) {
  await page.addInitScript(() => {
    localStorage.setItem('prx-console-token', 'prx_test_token');
  });
  await page.route('**/api/status', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        config_reload_status: 'idle',
        config_reload_in_progress: false,
        active_config_generation: 2,
        active_config_source_revision: 'rev-active',
        desired_config_source_revision: 'rev-desired',
        restart_required: [],
        config_generation_participants: ['gateway'],
        last_config_reload_failure: null
      })
    })
  );
}

test('channel send shows the response and does not invent delivery', async ({ page }) => {
  await boot(page);
  await page.route('**/api/channels/status', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        channels: [{ name: 'telegram', type: 'telegram', status: 'configured', enabled: true }]
      })
    })
  );
  await page.route('**/api/channels/telegram/send', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ channel: 'telegram', delivered: false, detail: 'queued' })
    })
  );
  await page.goto('/channels');
  await expect(page.getByText('Configured state: Configured')).toBeVisible();
  await page.getByRole('button', { name: 'Send' }).click();
  await expect(page.getByText('false')).toBeVisible();
  await expect(page.getByText('queued')).toBeVisible();
});

test('channel send denial is shown and not retried', async ({ page }) => {
  await boot(page);
  let sends = 0;
  await page.route('**/api/channels/status', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        channels: [{ name: 'telegram', type: 'telegram', status: 'configured', enabled: true }]
      })
    })
  );
  await page.route('**/api/channels/telegram/send', (route) => {
    sends += 1;
    return route.fulfill({
      status: 403,
      contentType: 'application/json',
      body: JSON.stringify({ error: 'send refused' })
    });
  });
  await page.goto('/channels');
  await page.getByRole('button', { name: 'Send' }).click();
  await expect(page.getByText('send refused')).toBeVisible();
  expect(sends).toBe(1);
});

test('an aborted channel send stays uncertain and keeps the draft', async ({ page }) => {
  await boot(page);
  await page.route('**/api/channels/status', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        channels: [{ name: 'telegram', type: 'telegram', status: 'configured', enabled: true }]
      })
    })
  );
  await page.route('**/api/channels/telegram/send', (route) => route.abort('timedout'));
  await page.goto('/channels');
  await page.getByLabel('Recipient').fill('prx_user');
  await page.getByRole('button', { name: 'Send' }).click();
  await expect(page.getByText(/was not sent again/)).toBeVisible();
  await expect(page.getByLabel('Recipient')).toHaveValue('prx_user');
});

test('plugin enable shows success, denial and conflict', async ({ page }) => {
  await boot(page);
  await page.route('**/api/plugins', (route) => {
    if (route.request().method() !== 'GET') return route.fallback();
    return route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ plugins: [], count: 0, generation: 1 })
    });
  });
  await page.route('**/api/plugins/voice/enable', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ plugin: { name: 'voice' }, enabled: true, generation: 2 })
    })
  );
  await page.goto('/plugins');
  await page.getByLabel('Disabled plugin name').fill('voice');
  await page.getByRole('button', { name: 'Enable' }).click();
  await expect(page.getByText(/"enabled": true/)).toBeVisible();

  await page.route('**/api/plugins/voice/enable', (route) =>
    route.fulfill({
      status: 403,
      contentType: 'application/json',
      body: JSON.stringify({ error: 'enable refused' })
    })
  );
  await page.getByRole('button', { name: 'Enable' }).click();
  await expect(page.getByText('enable refused')).toBeVisible();

  await page.route('**/api/plugins/voice/enable', (route) =>
    route.fulfill({
      status: 409,
      contentType: 'application/json',
      body: JSON.stringify({ error: 'plugin is already enabled' })
    })
  );
  await page.getByRole('button', { name: 'Enable' }).click();
  await expect(page.getByText('plugin is already enabled')).toBeVisible();
});

test('a stale rollback asks for confirmation and a plugin removal can still be refused', async ({ page }) => {
  await boot(page);
  await page.route('**/api/config/history', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        revision: 'rev-1',
        records: [
          {
            id: 'rec-1',
            created_at: '2026-10-31T00:00:00Z',
            actor: 'prx_user',
            status: 'applied',
            summary: { risk: 'low' },
            plan: 'plan text'
          }
        ]
      })
    })
  );
  await page.route('**/api/config/rollback', (route) =>
    route.fulfill({
      status: 428,
      contentType: 'application/json',
      body: JSON.stringify({
        error: 'stale revision',
        required: 'confirm',
        revision: 'rev-2',
        plan: 'restore plan',
        undoes: 'undo plan',
        risk_reasons: []
      })
    })
  );
  await page.goto('/changes');
  await page.getByRole('button', { name: 'Roll back' }).click();
  await expect(page.getByText('Confirm this rollback.')).toBeVisible();
  await expect(page.getByText('restore plan')).toBeVisible();

  await page.route('**/api/plugins', (route) => {
    if (route.request().method() !== 'GET') return route.fallback();
    return route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        plugins: [
          {
            name: 'voice',
            version: '1',
            description: '',
            capabilities: [],
            status: 'Active',
            permissions_required: [],
            permissions_granted: []
          }
        ],
        count: 1,
        generation: 1
      })
    });
  });
  await page.route('**/api/plugins/voice?**', (route) =>
    route.fulfill({
      status: 428,
      contentType: 'application/json',
      body: JSON.stringify({
        error: 'high-risk plugin change requires confirmation',
        required: 'confirm_high_risk',
        risk: 'high'
      })
    })
  );
  await page.goto('/plugins');
  await page.getByRole('button', { name: 'Remove' }).click();
  await page.getByRole('button', { name: 'Confirm' }).click();
  await expect(page.getByText(/needs confirmation/)).toBeVisible();
});
