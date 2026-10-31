import { afterEach, expect, test, vi } from 'vitest';
import { api } from './api';

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

function jsonResponse(body, status = 200) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/json' }
  });
}

test('sendChannel posts recipient, message and as_voice', async () => {
  const signal = new AbortController().signal;
  const fetchMock = vi.fn(async () => jsonResponse({ channel: 'telegram', delivered: false, detail: 'queued' }));
  vi.stubGlobal('fetch', fetchMock);

  const body = await api.sendChannel('tele gram', { recipient: 'prx_user', message: 'hello', asVoice: true }, { signal });

  expect(body).toEqual({ channel: 'telegram', delivered: false, detail: 'queued' });
  const [url, init] = fetchMock.mock.calls[0];
  expect(url).toBe('/api/channels/tele%20gram/send');
  expect(init.method).toBe('POST');
  expect(init.signal).toBe(signal);
  expect(JSON.parse(init.body)).toEqual({ recipient: 'prx_user', message: 'hello', as_voice: true });
});

test('sendChannel times out when the caller does not supply a signal', async () => {
  const signal = new AbortController().signal;
  const timeout = vi.spyOn(AbortSignal, 'timeout').mockReturnValue(signal);
  vi.stubGlobal('fetch', vi.fn(async () => jsonResponse({ channel: 'telegram', delivered: false, detail: '' })));

  await api.sendChannel('telegram', { recipient: 'prx_user', message: 'hello' });

  expect(timeout).toHaveBeenCalledWith(20_000);
});

test('reloadConfig posts an empty object', async () => {
  const fetchMock = vi.fn(async () => jsonResponse({ success: true, message: '{"status":"idle"}' }));
  vi.stubGlobal('fetch', fetchMock);

  await api.reloadConfig();

  const [url, init] = fetchMock.mock.calls[0];
  expect(url).toBe('/api/config/reload');
  expect(init.method).toBe('POST');
  expect(init.body).toBe('{}');
});

test('toggleSkill patches the skill id and sends no body', async () => {
  const fetchMock = vi.fn(async () => jsonResponse({ status: 'ok', id: 'skill-a', enabled: true }));
  vi.stubGlobal('fetch', fetchMock);

  const body = await api.toggleSkill('skill a');

  expect(body).toEqual({ status: 'ok', id: 'skill-a', enabled: true });
  const [url, init] = fetchMock.mock.calls[0];
  expect(url).toBe('/api/skills/skill%20a/toggle');
  expect(init.method).toBe('PATCH');
  expect(init.body).toBeUndefined();
});

test('plugin mutations use the typed routes and confirm high risk writes', async () => {
  const fetchMock = vi.fn(async () => jsonResponse({ enabled: true }));
  vi.stubGlobal('fetch', fetchMock);
  const reviewed = {
    source: 'plugin-sources/voice',
    sha256: 'abc',
    manifest_sha256: 'def',
    permissions: ['log']
  };

  await api.enablePlugin('voice talk');
  await api.disablePlugin('voice talk');
  await api.refreshPlugins();
  await api.installPlugin(reviewed);
  await api.updatePlugin(reviewed);
  await api.removePlugin('voice talk');

  const calls = fetchMock.mock.calls.map(([url, init]) => [url, init.method, init.body]);
  expect(calls).toEqual([
    ['/api/plugins/voice%20talk/enable', 'POST', '{}'],
    ['/api/plugins/voice%20talk/disable', 'POST', '{}'],
    ['/api/plugins/refresh', 'POST', '{}'],
    ['/api/plugins/install?confirm_high_risk=true', 'POST', JSON.stringify(reviewed)],
    ['/api/plugins/update?confirm_high_risk=true', 'POST', JSON.stringify(reviewed)],
    ['/api/plugins/voice%20talk?confirm_high_risk=true', 'DELETE', undefined]
  ]);
});
