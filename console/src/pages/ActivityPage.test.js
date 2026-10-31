import { fireEvent, render, screen } from '@testing-library/svelte';
import { beforeEach, expect, test, vi } from 'vitest';
import ActivityPage from './ActivityPage.svelte';
import { ApiError, api } from '../lib/api';

vi.mock('../lib/api', () => ({
  ApiError: class ApiError extends Error {
    constructor(status, message) {
      super(message);
      this.status = status;
    }
  },
  api: {
    listJobs: vi.fn(),
    getJob: vi.fn(),
    cancelJob: vi.fn(),
    listTasks: vi.fn(),
    getPools: vi.fn(),
    killTask: vi.fn(),
    steerTask: vi.fn()
  }
}));

beforeEach(() => {
  api.listJobs.mockReset();
  api.getJob.mockReset();
  api.cancelJob.mockReset();
  api.listTasks.mockReset();
  api.getPools.mockReset();
});

test('lists jobs for this process and shows a denied activity state', async () => {
  api.listJobs.mockResolvedValueOnce({
    jobs: [{
      job_id: 'job-1',
      status: 'running',
      label: 'gateway:session_message:session-1',
      work_id: 'work-1',
      elapsed_ms: 15
    }],
    total: 1
  });
  const view = render(ActivityPage);
  expect((await screen.findAllByText('gateway:session_message:session-1')).length).toBeGreaterThan(0);
  expect(screen.getByText(/process-wide/)).toBeInTheDocument();
  view.unmount();
  api.listJobs.mockRejectedValueOnce(new ApiError(403, 'no'));
  render(ActivityPage);
  expect(await screen.findByText('You do not have permission to view activity.')).toBeInTheDocument();
});

test('keeps the cancel acknowledgement after the detail refresh', async () => {
  api.listJobs.mockResolvedValue({
    jobs: [{ job_id: 'job-1', status: 'running', label: 'gateway:session_message:session-1', work_id: 'work-1', elapsed_ms: 15 }],
    total: 1
  });
  api.getJob.mockResolvedValue({
    job_id: 'job-1', status: 'running', work_id: 'work-1', http_status: null, result: null, error: null, elapsed_ms: 15
  });
  api.cancelJob.mockResolvedValue({ job_id: 'job-1', work_id: 'work-1', targets: ['work-1:requested'] });
  render(ActivityPage);
  await screen.findAllByText('gateway:session_message:session-1');
  const selects = screen.getAllByRole('combobox');
  await fireEvent.change(selects[selects.length - 1], { target: { value: 'job-1' } });
  await fireEvent.click(await screen.findByRole('button', { name: 'Cancel turn' }));
  await fireEvent.click(screen.getByRole('button', { name: 'Confirm' }));
  expect(await screen.findByText(/not proof the process has exited/)).toBeInTheDocument();
  expect(api.cancelJob).toHaveBeenCalledTimes(1);
});
