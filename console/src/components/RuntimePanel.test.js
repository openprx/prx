import { fireEvent, render, screen } from '@testing-library/svelte';
import { beforeEach, expect, test, vi } from 'vitest';
import RuntimePanel from './RuntimePanel.svelte';
import { ApiError, api } from '../lib/api';

vi.mock('../lib/api', () => ({
  ApiError: class ApiError extends Error {
    constructor(status, message, body = null) {
      super(message);
      this.status = status;
      this.body = body;
    }
  },
  api: {
    listTasks: vi.fn(),
    getPools: vi.fn(),
    killTask: vi.fn(),
    steerTask: vi.fn()
  }
}));

beforeEach(() => {
  api.listTasks.mockReset();
  api.getPools.mockReset();
  api.steerTask.mockReset();
});

test('shows a 409 steer refusal on the row and keeps the page available', async () => {
  api.listTasks.mockResolvedValue({
    running: [{ id: 'w42', kind: 'task', name: 'prx_node', state: 'running', parent: null, pid: 7, steerable: false }],
    unreaped: [],
    total: 1
  });
  api.getPools.mockResolvedValue({ pools: [{ kind: 'http', name: 'default', metrics: { idle: 1 } }] });
  api.steerTask.mockRejectedValue(new ApiError(409, 'work item has no steering channel'));
  render(RuntimePanel);
  expect(await screen.findByText('w42')).toBeInTheDocument();
  const message = screen.getByRole('textbox');
  await fireEvent.input(message, { target: { value: 'hold' } });
  await fireEvent.click(screen.getByRole('button', { name: 'Send to task' }));
  await fireEvent.click(await screen.findByRole('button', { name: 'Confirm' }));
  expect(await screen.findByText('work item has no steering channel')).toBeInTheDocument();
  expect(screen.getByText('Pools')).toBeInTheDocument();
});
