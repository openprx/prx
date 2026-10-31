import { render, screen } from '@testing-library/svelte';
import { beforeEach, expect, test, vi } from 'vitest';
import TerminalSessionsPage from './TerminalSessionsPage.svelte';
import { api } from '../lib/api';

vi.mock('../lib/api', () => ({
  ApiError: class ApiError extends Error {
    constructor(status, message) {
      super(message);
      this.status = status;
    }
  },
  api: {
    listChatSessions: vi.fn(),
    listChatResults: vi.fn(),
    assignChatSession: vi.fn()
  }
}));

beforeEach(() => {
  api.listChatSessions.mockReset();
  api.listChatResults.mockReset();
});

test('lists a mailbox without a token and warns when older results were dropped', async () => {
  api.listChatSessions.mockResolvedValue({
    sessions: [{
      session_id: 'mailbox-1',
      label: 'prx_node',
      liveness: 'polling',
      pid: 42,
      queued: 1,
      token: 'must-not-render'
    }]
  });
  api.listChatResults.mockResolvedValue({
    results: [{ seq: 4, session_id: 'mailbox-1', session_label: 'prx_node', disposition: 'queue', status: 'completed', summary: 'done' }],
    next_seq: 5,
    oldest_seq: 4
  });
  render(TerminalSessionsPage);
  expect(await screen.findByText('mailbox-1')).toBeInTheDocument();
  expect(screen.getByText(/not the conversations stored by this console/)).toBeInTheDocument();
  expect(screen.queryByText('must-not-render')).not.toBeInTheDocument();
  expect(await screen.findByText('Some older results are no longer retained.')).toBeInTheDocument();
  expect(screen.getByText(/done/)).toBeInTheDocument();
});
