import { render, screen } from '@testing-library/svelte';
import { beforeEach, expect, test, vi } from 'vitest';

vi.mock('../lib/api', () => ({
  api: { getMcpServers: vi.fn() }
}));

import { api } from '../lib/api';
import McpPage from './McpPage.svelte';

beforeEach(() => {
  api.getMcpServers.mockReset();
});

test('says connect, restart and delete are unavailable', async () => {
  api.getMcpServers.mockResolvedValue({ servers: [] });
  render(McpPage);
  expect(await screen.findByText(/Connect, restart and delete are not available/)).toBeInTheDocument();
  expect(screen.getByText('No MCP servers configured.')).toBeInTheDocument();
});

test('shows a denied MCP load', async () => {
  const error = new Error('mcp refused');
  error.status = 403;
  api.getMcpServers.mockRejectedValue(error);
  render(McpPage);
  const alert = await screen.findByRole('alert');
  expect(alert).toHaveTextContent('mcp refused');
  expect(alert.closest('.ui-state-denied')).toBeTruthy();
});
