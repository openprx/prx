import { fireEvent, render, screen } from '@testing-library/svelte';
import { beforeEach, expect, test, vi } from 'vitest';
import DecisionApprovalPanel from './DecisionApprovalPanel.svelte';
import { ApiError, api } from '../lib/api';

const DECISION_ID = '11111111-1111-4111-8111-111111111111';

vi.mock('../lib/api', () => ({
  ApiError: class ApiError extends Error {
    constructor(status, message) {
      super(message);
      this.status = status;
    }
  },
  api: {
    getDecisionApproval: vi.fn(),
    reviewDecisionApproval: vi.fn(),
    revokeDecisionApproval: vi.fn(),
    resumeDecisionApprovalAsync: vi.fn(),
    getJobByPath: vi.fn()
  }
}));

beforeEach(() => {
  api.getDecisionApproval.mockReset();
  api.resumeDecisionApprovalAsync.mockReset();
  api.getJobByPath.mockReset();
});

test('a fresh inspect clears a missing resume job and enables the actions', async () => {
  api.getDecisionApproval.mockResolvedValue({
    decision_id: DECISION_ID,
    tool: 'file_write',
    effect: 'write',
    call_digest: 'digest',
    pending_expires_at: '2099-01-01T00:00:00Z',
    arguments: {},
    arguments_redacted: false,
    original_call_available: true,
    single_use: true
  });
  api.resumeDecisionApprovalAsync.mockResolvedValue({
    status: 202,
    body: {
      status: 'accepted',
      job_id: 'job-resume',
      work_id: 'work-resume',
      poll_url: '/api/jobs/job-resume'
    }
  });
  api.getJobByPath.mockRejectedValue(new ApiError(404, 'missing'));
  render(DecisionApprovalPanel, { props: { sessionId: 'session-1' } });
  await fireEvent.click(screen.getByText('Review a pending operation'));
  await fireEvent.input(screen.getByLabelText('Decision identifier'), { target: { value: DECISION_ID } });
  await fireEvent.click(screen.getByRole('button', { name: 'Load details' }));
  const resume = await screen.findByRole('button', { name: 'Run approved call' });
  expect(resume).toBeEnabled();
  await fireEvent.click(resume);
  expect(await screen.findByText('The gateway no longer knows this job; check history.')).toBeInTheDocument();
  expect(screen.getByRole('button', { name: 'Run approved call' })).toBeDisabled();
  await fireEvent.click(screen.getByRole('button', { name: 'Load details' }));
  expect(await screen.findByRole('button', { name: 'Run approved call' })).toBeEnabled();
});
