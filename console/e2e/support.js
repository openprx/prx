export const statusBody = {
  version: '1.2.3',
  active_config_generation: 1,
  active_config_source_revision: '0123456789abcdef',
  desired_config_source_revision: '0123456789abcdef',
  config_reload_status: 'idle',
  config_reload_in_progress: false,
  restart_required: [],
  provider_degraded: false
};

export async function signIn(page) {
  await page.addInitScript(() => {
    localStorage.setItem('prx-console-token', 'prx_test_token');
  });
}

export async function stubStatus(page) {
  await page.route('**/api/status', (route) => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: JSON.stringify(statusBody)
  }));
}

export function fulfillJson(route, status, body) {
  return route.fulfill({
    status,
    contentType: 'application/json',
    body: JSON.stringify(body)
  });
}
