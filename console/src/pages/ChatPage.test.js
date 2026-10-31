import { fireEvent, render, screen } from '@testing-library/svelte';
import { beforeEach, expect, test, vi } from 'vitest';
import ChatPage from './ChatPage.svelte';
import { ApiError, api } from '../lib/api';

vi.mock('../lib/api', () => ({
  ApiError: class ApiError extends Error {
    constructor(status, message) {
      super(message);
      this.status = status;
    }
  },
  api: {
    getSessionMessages: vi.fn(),
    sendTurn: vi.fn(),
    sendTurnWithMedia: vi.fn(),
    getJobByPath: vi.fn(),
    postByApiPath: vi.fn(),
    getDecisionApproval: vi.fn(),
    reviewDecisionApproval: vi.fn(),
    revokeDecisionApproval: vi.fn(),
    resumeDecisionApprovalAsync: vi.fn()
  }
}));

beforeEach(() => {
  api.getSessionMessages.mockReset();
  api.sendTurn.mockReset();
  api.getJobByPath.mockReset();
  api.getSessionMessages.mockResolvedValue([]);
});

test('a missing job enables the composer and restores the draft', async () => {
  api.sendTurn.mockResolvedValue({
    status: 202,
    body: {
      status: 'accepted',
      job_id: 'job-gone',
      work_id: 'work-gone',
      poll_url: '/api/jobs/job-gone',
      cancel_url: '/api/jobs/job-gone/cancel'
    }
  });
  api.getJobByPath.mockRejectedValue(new ApiError(404, 'missing'));
  render(ChatPage, { props: { sessionId: 'session-1' } });
  const box = await screen.findByPlaceholderText('Type a message...');
  await fireEvent.input(box, { target: { value: 'keep this draft' } });
  await fireEvent.click(screen.getByRole('button', { name: 'Send' }));
  expect(await screen.findByText('The gateway no longer knows this job; check history.')).toBeInTheDocument();
  expect(box).toHaveValue('keep this draft');
  expect(screen.getByRole('button', { name: 'Send' })).toBeEnabled();
  expect(screen.getByTitle('Attach files')).toBeEnabled();
  expect(screen.queryByRole('button', { name: 'Cancel turn' })).not.toBeInTheDocument();
});
