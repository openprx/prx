import { fireEvent, render, screen } from '@testing-library/svelte';
import { beforeEach, expect, test, vi } from 'vitest';
import DecisionApprovalPanel from './DecisionApprovalPanel.svelte';
import { api } from '../lib/api';

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
    revokeDecisionApproval: vi.fn()
  }
}));

beforeEach(() => {
  api.getDecisionApproval.mockReset();
  api.reviewDecisionApproval.mockReset();
  api.revokeDecisionApproval.mockReset();
});

test('an approval is recorded without running the call and offers no resume', async () => {
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
  api.reviewDecisionApproval.mockResolvedValue({
    decision_id: DECISION_ID,
    approved: true,
    grant_count: 1,
    expires_at: '2099-01-01T00:00:00Z'
  });
  render(DecisionApprovalPanel, { props: { sessionId: 'session-1' } });
  await fireEvent.click(screen.getByText('Review a pending operation'));
  await fireEvent.input(screen.getByLabelText('Decision identifier'), { target: { value: DECISION_ID } });
  await fireEvent.click(screen.getByRole('button', { name: 'Load details' }));
  const approve = await screen.findByRole('button', { name: 'Approve once' });
  expect(screen.queryByRole('button', { name: /Run approved call/ })).toBeNull();
  await fireEvent.click(approve);
  expect(await screen.findByText('Approval recorded. The operation has not run.')).toBeInTheDocument();
  expect(api.reviewDecisionApproval).toHaveBeenCalledWith('session-1', DECISION_ID, true);
});
