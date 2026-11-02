import { afterEach, beforeEach, expect, test, vi } from 'vitest';

vi.mock('./auth.js', () => ({
  getToken: () => 'prx_test_token',
  clearToken: vi.fn()
}));

vi.mock('./router.js', () => ({
  navigate: vi.fn(),
  currentPath: () => '/'
}));

import { api } from './api';

afterEach(() => {
  vi.unstubAllGlobals();
});

function jsonResponse(status, body, headers = { 'content-type': 'application/json' }) {
  return new Response(JSON.stringify(body), { status, headers });
}

test('createSession posts an empty object and returns the status', async () => {
  const fetchMock = vi.fn(async (_url, init) => {
    expect(init.method).toBe('POST');
    expect(init.body).toBe('{}');
    return jsonResponse(201, { session_id: 'session-1', status: 'empty', message_count: 0 });
  });
  vi.stubGlobal('fetch', fetchMock);
  const result = await api.createSession();
  expect(fetchMock.mock.calls[0][0]).toBe('/api/sessions');
  expect(result.status).toBe(201);
  expect(result.body.session_id).toBe('session-1');
});

test('sendTurn keeps the async preference and a media body without a content type', async () => {
  const fetchMock = vi.fn(async () => jsonResponse(202, { status: 'accepted', job_id: 'job-1' }));
  vi.stubGlobal('fetch', fetchMock);
  await api.sendTurn('session 1', 'hello');
  const [, init] = fetchMock.mock.calls[0];
  expect(init.headers.Prefer).toBe('respond-async');
  expect(init.body).toBe(JSON.stringify({ message: 'hello' }));
  const file = new File(['a'], 'note.txt', { type: 'text/plain' });
  await api.sendTurnWithMedia('session 1', 'hello', [file]);
  const mediaInit = fetchMock.mock.calls[1][1];
  expect(mediaInit.body).toBeInstanceOf(FormData);
  expect(mediaInit.headers['Content-Type']).toBeUndefined();
  expect(mediaInit.headers.Prefer).toBe('respond-async');
});

test('assignChatSession sends only task and disposition', async () => {
  const fetchMock = vi.fn(async () => jsonResponse(202, { assignment_id: 'a1' }));
  vi.stubGlobal('fetch', fetchMock);
  await api.assignChatSession('box', { task: 'inspect', disposition: 'queue', on_behalf_of: { channel: 'x' }, token: 'secret' });
  const body = JSON.parse(fetchMock.mock.calls[0][1].body);
  expect(body).toEqual({ task: 'inspect', disposition: 'queue' });
});

test('listApprovals and job reads use the caller path', async () => {
  const fetchMock = vi.fn(async () => jsonResponse(200, { approvals: [], jobs: [] }));
  vi.stubGlobal('fetch', fetchMock);
  await api.listApprovals({ limit: 20, cursor: '4' });
  await api.getJob('job-1');
  await api.cancelJob('job-1');
  await api.listTasks();
  await api.killTask('w42', false);
  await api.steerTask('run-1', 'hold');
  await api.getPools();
  await api.listChatSessions();
  await api.listChatResults({ afterSeq: 3, limit: 10 });
  await api.resumeDecisionApprovalAsync('s1', 'd1');
  const urls = fetchMock.mock.calls.map((call) => call[0]);
  expect(urls[0]).toBe('/api/sessions/approvals?limit=20&cursor=4');
  expect(urls[1]).toBe('/api/jobs/job-1');
  expect(urls[2]).toBe('/api/jobs/job-1/cancel');
  expect(urls[3]).toBe('/api/runtime/tasks');
  expect(urls[4]).toBe('/api/runtime/tasks/w42/kill?cascade=false');
  expect(urls[5]).toBe('/api/runtime/tasks/run-1/message');
  expect(JSON.parse(fetchMock.mock.calls[5][1].body)).toEqual({ message: 'hold' });
  expect(urls[6]).toBe('/api/runtime/pools');
  expect(urls[7]).toBe('/api/chat-sessions');
  expect(urls[8]).toBe('/api/chat-sessions/results?after_seq=3&limit=10');
  expect(fetchMock.mock.calls[9][1].headers.Prefer).toBe('respond-async');
});

test('refuses a job path that is not a same-origin API path', async () => {
  vi.stubGlobal('fetch', vi.fn());
  await expect(api.getJobByPath('https://example.com/api/jobs/1')).rejects.toMatchObject({ status: 400 });
  await expect(api.postByApiPath('//example.com/api/jobs/1/cancel')).rejects.toMatchObject({ status: 400 });
});

beforeEach(() => {
  vi.stubGlobal(
    'fetch',
    vi.fn(async () => ({
      ok: true,
      status: 200,
      headers: { get: () => 'application/json' },
      json: async () => ({ ok: true })
    }))
  );
});

const calls = [
  ['listCronJobs', [], '/api/cron/jobs', 'GET'],
  ['getCronJob', ['job/1'], '/api/cron/jobs/job%2F1', 'GET'],
  ['listCronRuns', ['job 1'], '/api/cron/jobs/job%201/runs', 'GET'],
  ['getCronStatus', [], '/api/cron/status', 'GET'],
  ['createCronJob', [{ command: 'true', every_ms: 60000 }], '/api/cron/jobs', 'POST'],
  ['updateCronJob', ['job-1', { command: 'true' }], '/api/cron/jobs/job-1', 'PATCH'],
  ['pauseCronJob', ['job-1'], '/api/cron/jobs/job-1/pause', 'POST'],
  ['resumeCronJob', ['job-1'], '/api/cron/jobs/job-1/resume', 'POST'],
  ['deleteCronJob', ['job-1'], '/api/cron/jobs/job-1', 'DELETE'],
  ['runCronJob', ['job-1'], '/api/cron/jobs/job-1/run', 'POST'],
  ['searchMemory', [{ query: 'notes' }], '/api/memory/search', 'POST'],
  ['recallMemory', [{ query: 'notes' }], '/api/memory/recall', 'POST'],
  ['storeMemory', [{ key: 'k', content: 'c' }], '/api/memory/store', 'POST'],
  ['forgetMemory', [{ key: 'k' }], '/api/memory/forget', 'POST'],
  ['getCost', ['today'], '/api/cost?window=today', 'GET'],
  ['getLifecycleStatus', [], '/api/skills/lifecycle/status', 'GET'],
  ['listLifecycleProposals', ['skill a'], '/api/skills/lifecycle/proposals?skill_id=skill+a', 'GET'],
  ['getLifecycleProposal', ['p/1'], '/api/skills/lifecycle/proposals/p%2F1', 'GET'],
  ['publishLifecycleProposal', ['p1'], '/api/skills/lifecycle/proposals/p1/publish', 'POST'],
  ['cancelLifecycleProposal', ['p1', 'done'], '/api/skills/lifecycle/proposals/p1/cancel', 'POST'],
  ['rollbackLifecycle', [{ skill_id: 's', revision_id: 'r', expected_generation: 2 }], '/api/skills/lifecycle/rollback', 'POST'],
  ['postLifecycleExperiment', ['lifecycle_experiment_catalog', { sample: true }], '/api/skills/lifecycle/h2', 'POST']
];

test.each(calls)('%s uses its route and does not send owner_id', async (method, args, path, verb) => {
  await api[method](...args);
  const [url, init] = fetch.mock.calls.at(-1);
  expect(url).toContain(path);
  expect(init.method ?? 'GET').toBe(verb);
  const serialized = typeof init.body === 'string' ? init.body : '';
  expect(serialized).not.toContain('owner_id');
  if (serialized) {
    expect(JSON.parse(serialized).owner_id).toBeUndefined();
  }
});
