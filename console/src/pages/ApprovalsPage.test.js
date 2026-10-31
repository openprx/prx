import { render, screen } from '@testing-library/svelte';
import { beforeEach, expect, test, vi } from 'vitest';
import ApprovalsPage from './ApprovalsPage.svelte';
import { ApiError, api } from '../lib/api';

vi.mock('../lib/api', () => ({
  ApiError: class ApiError extends Error {
    constructor(status, message) {
      super(message);
      this.status = status;
    }
  },
  api: { listApprovals: vi.fn() }
}));

beforeEach(() => {
  api.listApprovals.mockReset();
});

test('shows an empty inbox separately from a disabled plane', async () => {
  api.listApprovals.mockResolvedValueOnce({ approvals: [], next_cursor: null });
  const view = render(ApprovalsPage);
  expect(await screen.findByText('No pending approvals.')).toBeInTheDocument();
  view.unmount();
  api.listApprovals.mockRejectedValueOnce(new ApiError(409, 'disabled'));
  render(ApprovalsPage);
  expect(await screen.findByText('Decision approvals are disabled.')).toBeInTheDocument();
});

test('shows a denied inbox and a pending row link', async () => {
  api.listApprovals.mockRejectedValueOnce(new ApiError(403, 'no'));
  const denied = render(ApprovalsPage);
  expect(await screen.findByText('You do not have permission to view approvals.')).toBeInTheDocument();
  denied.unmount();
  api.listApprovals.mockResolvedValueOnce({
    approvals: [{
      session_id: 'session-1',
      decision_id: '11111111-1111-4111-8111-111111111111',
      tool: 'file_read',
      effect: 'read',
      arguments_redacted: true,
      pending_expires_at: '2026-10-31T00:00:00Z'
    }],
    next_cursor: null
  });
  render(ApprovalsPage);
  expect(await screen.findByRole('button', { name: 'Open session' })).toBeInTheDocument();
  expect(screen.getByText(/Arguments redacted/)).toBeInTheDocument();
});
