import { fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import { beforeEach, expect, test, vi } from 'vitest';

const { api, ApiError } = vi.hoisted(() => {
  class ApiError extends Error {
    constructor(status, message, body = null) {
      super(message);
      this.name = 'ApiError';
      this.status = status;
      this.body = body;
    }
  }
  return {
    ApiError,
    api: {
      getStatus: vi.fn(),
      listCronJobs: vi.fn(),
      getCronStatus: vi.fn(),
      getCronJob: vi.fn(),
      listCronRuns: vi.fn(),
      createCronJob: vi.fn(),
      updateCronJob: vi.fn(),
      pauseCronJob: vi.fn(),
      resumeCronJob: vi.fn(),
      deleteCronJob: vi.fn(),
      runCronJob: vi.fn(),
      searchMemory: vi.fn(),
      getCost: vi.fn(),
      getLifecycleStatus: vi.fn(),
      listLifecycleProposals: vi.fn(),
      getLifecycleProposal: vi.fn(),
      publishLifecycleProposal: vi.fn(),
      cancelLifecycleProposal: vi.fn(),
      rollbackLifecycle: vi.fn(),
      postLifecycleExperiment: vi.fn(),
      getDecisionStatus: vi.fn()
    }
  };
});

vi.mock('../lib/api.js', () => ({ api, ApiError }));
vi.mock('../lib/router.js', () => ({
  navigate: vi.fn(),
  currentPath: () => '/schedules'
}));

import CostPage from './CostPage.svelte';
import DecisionPlanePage from './DecisionPlanePage.svelte';
import LogsPage from './LogsPage.svelte';
import MemoryPage from './MemoryPage.svelte';
import OverviewPage from './OverviewPage.svelte';
import SchedulesPage from './SchedulesPage.svelte';
import SkillLifecycleH2Page from './SkillLifecycleH2Page.svelte';
import SkillLifecyclePage from './SkillLifecyclePage.svelte';

beforeEach(() => {
  vi.clearAllMocks();
  vi.stubGlobal(
    'WebSocket',
    class FakeSocket {
      constructor() {
        this.readyState = 0;
      }
      close() {}
    }
  );
});

test('overview shows health values and unknown when a status field is absent', async () => {
  api.getStatus.mockResolvedValue({
    version: '1.2.3',
    uptime_seconds: 12,
    model: 'prx-model',
    memory_backend: 'markdown',
    gateway_port: 8080,
    channels: [],
    restart_required: ['channels'],
    provider_degraded: false,
    config_generation_participants: [],
    provider_unavailable: ['remote']
  });
  render(OverviewPage);
  expect(await screen.findByText('Generation')).toBeInTheDocument();
  expect(screen.getAllByText('Unknown').length).toBeGreaterThan(0);
  expect(screen.getByText('Required: channels')).toBeInTheDocument();
  expect(screen.getByText('Available')).toBeInTheDocument();
  expect(screen.getByText('None')).toBeInTheDocument();
  expect(screen.getByText('remote')).toBeInTheDocument();
});

test('schedules add a shell job and run now reloads runs once', async () => {
  api.listCronJobs.mockResolvedValue({
    jobs: [{ id: 'job-1', name: 'digest', command: 'true', enabled: true, schedule: { kind: 'every', every_ms: 60000 } }]
  });
  api.getCronStatus.mockResolvedValue({ enabled: true, disabled_state: 'not_configured' });
  api.createCronJob.mockResolvedValue({ id: 'job-2' });
  const view = render(SchedulesPage, { props: { path: '/schedules' } });
  expect(await screen.findByRole('button', { name: 'Open digest' })).toBeInTheDocument();
  expect(screen.getByText(/mark-operator stays on the CLI/)).toBeInTheDocument();
  await fireEvent.change(screen.getByLabelText('Schedule'), { target: { value: 'every' } });
  await fireEvent.input(screen.getByLabelText('Every (milliseconds)'), { target: { value: '60000' } });
  await fireEvent.input(screen.getByLabelText('Shell command'), { target: { value: 'true' } });
  await fireEvent.click(screen.getByRole('button', { name: 'Add job' }));
  await waitFor(() => expect(api.createCronJob).toHaveBeenCalledWith({ command: 'true', every_ms: 60000 }));
  view.unmount();

  api.getCronJob.mockResolvedValue({
    id: 'job-1',
    enabled: true,
    command: 'true',
    schedule: { kind: 'every', every_ms: 60000 },
    events: [{ event_type: 'added', status: 'ok', created_at: '2026-10-31T00:00:00Z' }]
  });
  api.listCronRuns.mockResolvedValue({ runs: [] });
  api.runCronJob.mockResolvedValue({ job_id: 'job-1', status: 'ok', duration_ms: 3, output: 'done' });
  render(SchedulesPage, { props: { path: '/schedules/job-1' } });
  await fireEvent.click(await screen.findByRole('button', { name: 'Run now' }));
  await waitFor(() => expect(api.runCronJob).toHaveBeenCalledTimes(1));
  await waitFor(() => expect(api.listCronRuns.mock.calls.length).toBeGreaterThanOrEqual(2));
  expect(await screen.findByText('done')).toBeInTheDocument();
});

test('a forbidden schedule list is denied', async () => {
  api.listCronJobs.mockRejectedValue(new ApiError(403, 'Security policy: read-only mode'));
  api.getCronStatus.mockRejectedValue(new ApiError(403, 'Security policy: read-only mode'));
  render(SchedulesPage, { props: { path: '/schedules' } });
  expect(await screen.findByText('Security policy: read-only mode')).toBeInTheDocument();
});

test('a schedule run conflict stays an error', async () => {
  api.getCronJob.mockResolvedValue({ id: 'job-1', enabled: false, schedule: { kind: 'cron', expr: '0 * * * *' }, events: [] });
  api.listCronRuns.mockResolvedValue({ runs: [] });
  api.runCronJob.mockRejectedValue(new ApiError(409, 'The job changed or is already running.'));
  render(SchedulesPage, { props: { path: '/schedules/job-1' } });
  await fireEvent.click(await screen.findByRole('button', { name: 'Run now' }));
  expect(await screen.findByText('The job changed or is already running.')).toBeInTheDocument();
});

test('memory search shows an empty result and an unavailable backend separately', async () => {
  api.searchMemory.mockResolvedValue({ success: true, scope: 'owner', source: 'sqlite', empty: true, hits: [], output: 'No matches found' });
  const view = render(MemoryPage);
  expect(screen.getByRole('button', { name: 'Search' })).toBeInTheDocument();
  expect(screen.getByRole('button', { name: 'Store' })).toBeInTheDocument();
  expect(screen.getByRole('button', { name: 'Recall' })).toBeInTheDocument();
  expect(screen.getByRole('button', { name: 'Forget' })).toBeInTheDocument();
  await fireEvent.input(screen.getByLabelText('Search query'), { target: { value: 'notes' } });
  await fireEvent.click(screen.getByRole('button', { name: 'Search' }));
  expect(await screen.findByText('No memories matched.')).toBeInTheDocument();
  expect(screen.getByText(/Scope: owner/)).toBeInTheDocument();
  expect(screen.getByText(/Source: sqlite/)).toBeInTheDocument();
  expect(screen.queryByText('Unavailable in this deployment.')).toBeNull();
  view.unmount();

  api.searchMemory.mockRejectedValue(new ApiError(503, 'memory is unavailable'));
  render(MemoryPage);
  await fireEvent.input(screen.getByLabelText('Search query'), { target: { value: 'notes' } });
  await fireEvent.click(screen.getByRole('button', { name: 'Search' }));
  expect(await screen.findByText('memory is unavailable')).toBeInTheDocument();
  expect(screen.getByText(/reindex and prx migrate/)).toBeInTheDocument();
});

test('cost separates a settled zero from estimates and does not invent zero on failure', async () => {
  api.getCost.mockResolvedValue({
    scope: 'workspace',
    estimates: true,
    unpriced_usage: 'omitted',
    outstanding_reservation_micro_usd: 15,
    as_of_utc: '2026-10-31T00:00:00Z',
    selected: { cost_usd: 0, requests: 2 }
  });
  const view = render(CostPage);
  expect(await screen.findByText('Settled')).toBeInTheDocument();
  expect(screen.getByText('0')).toBeInTheDocument();
  expect(screen.getByText('15')).toBeInTheDocument();
  expect(screen.getAllByText(/workspace/).length).toBeGreaterThan(0);
  view.unmount();

  api.getCost.mockRejectedValue(new ApiError(500, 'workspace cost report could not be encoded'));
  render(CostPage);
  expect(await screen.findByText('workspace cost report could not be encoded')).toBeInTheDocument();
  expect(screen.queryByText('Settled')).toBeNull();
});

test('lifecycle shows a grant refusal and committed repair without calling it a rollback', async () => {
  api.getLifecycleStatus.mockResolvedValue({ action: 'lifecycle_status', enabled: true });
  api.listLifecycleProposals.mockResolvedValue({ proposals: [{ proposal_id: 'p1', skill_id: 'skill-a', status: 'open' }] });
  api.getLifecycleProposal.mockResolvedValue({ proposal_id: 'p1', status: 'open' });
  api.publishLifecycleProposal.mockRejectedValue(
    new ApiError(403, "publish and rollback need the operator's full session")
  );
  const view = render(SkillLifecyclePage);
  await fireEvent.click(await screen.findByRole('button', { name: 'Open proposal p1' }));
  await fireEvent.click(await screen.findByRole('button', { name: 'Publish' }));
  expect(await screen.findByText("publish and rollback need the operator's full session")).toBeInTheDocument();
  expect(screen.getByText(/not live or benefit acceptance/)).toBeInTheDocument();
  view.unmount();

  api.getLifecycleStatus.mockResolvedValue({ action: 'lifecycle_status', enabled: true });
  api.listLifecycleProposals.mockResolvedValue({ proposals: [] });
  api.rollbackLifecycle.mockRejectedValue(
    new ApiError(409, 'committed with repair required', { committed_repair_required: true })
  );
  render(SkillLifecyclePage);
  await screen.findByLabelText('Skill id');
  await fireEvent.input(screen.getByLabelText('Skill id'), { target: { value: 'skill-a' } });
  await fireEvent.input(screen.getByLabelText('Revision id'), { target: { value: 'rev-1' } });
  await fireEvent.input(screen.getByLabelText('Expected generation'), { target: { value: '3' } });
  await fireEvent.click(screen.getByRole('button', { name: 'Rollback' }));
  expect(await screen.findByText('Committed with repair required. This is not a rollback.')).toBeInTheDocument();
  expect(screen.queryByText(/rolled back/i)).toBeNull();
});

test('h2 offers the thirteen actions and surfaces the mint gap', async () => {
  api.postLifecycleExperiment.mockRejectedValue(new ApiError(503, 'the gateway cannot mint an H2 invocation'));
  render(SkillLifecycleH2Page);
  const select = screen.getByLabelText('Action');
  const labels = [...select.options].map((option) => option.value);
  expect(labels).toHaveLength(13);
  expect(labels.some((label) => label.includes('study'))).toBe(false);
  await fireEvent.input(screen.getByLabelText('Request JSON'), { target: { value: '{' } });
  await fireEvent.click(screen.getByRole('button', { name: 'Submit' }));
  expect(screen.getByText('Request JSON must be an object.')).toBeInTheDocument();
  await fireEvent.input(screen.getByLabelText('Request JSON'), { target: { value: '{}' } });
  await fireEvent.click(screen.getByRole('button', { name: 'Submit' }));
  await waitFor(() =>
    expect(api.postLifecycleExperiment).toHaveBeenCalledWith('lifecycle_experiment_catalog', {})
  );
  expect(await screen.findByText('the gateway cannot mint an H2 invocation')).toBeInTheDocument();
  expect(screen.getByText(/not live or benefit acceptance/)).toBeInTheDocument();
});

test('logs name the process socket and mark a disconnected stream stale', async () => {
  render(LogsPage);
  expect(await screen.findByText('These are process logs, not the job state machine.')).toBeInTheDocument();
  expect(screen.getByText('The process log socket is not connected. Lines on screen may be stale.')).toBeInTheDocument();
  expect(screen.getByRole('button', { name: 'Export redacted lines' })).toBeInTheDocument();
});

test('decision plane shows a loading state before status arrives', async () => {
  api.getDecisionStatus.mockReturnValue(new Promise(() => {}));
  render(DecisionPlanePage);
  expect(await screen.findByText('Loading...')).toBeInTheDocument();
  expect(screen.getByText(/CLI evaluate stays on the command line/)).toBeInTheDocument();
});
