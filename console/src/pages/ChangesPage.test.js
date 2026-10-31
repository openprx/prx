import { fireEvent, render, screen } from '@testing-library/svelte';
import { beforeEach, expect, test, vi } from 'vitest';

vi.mock('../lib/api', () => ({
  api: {
    getConfigHistory: vi.fn(),
    rollbackConfig: vi.fn(),
    reloadConfig: vi.fn(),
    getStatus: vi.fn()
  }
}));

import { api } from '../lib/api';
import ChangesPage from './ChangesPage.svelte';

beforeEach(() => {
  api.getConfigHistory.mockReset();
  api.rollbackConfig.mockReset();
  api.reloadConfig.mockReset();
  api.getStatus.mockReset();
  api.getConfigHistory.mockResolvedValue({ records: [], revision: 'rev-1' });
});

test('shows the reload response and the polled status fields', async () => {
  api.reloadConfig.mockResolvedValue({ success: true, message: '{"status":"published"}' });
  api.getStatus.mockResolvedValue({
    config_reload_status: 'idle',
    config_reload_in_progress: false,
    active_config_generation: 4,
    active_config_source_revision: 'rev-active',
    desired_config_source_revision: 'rev-desired',
    restart_required: ['channels'],
    config_generation_participants: ['gateway'],
    last_config_reload_failure: null
  });
  render(ChangesPage);
  await fireEvent.click(await screen.findByRole('button', { name: 'Reload configuration' }));
  expect(await screen.findByText(/HTTP 200 does not mean every participant reloaded/)).toBeInTheDocument();
  expect(await screen.findByText('{"status":"published"}')).toBeInTheDocument();
  expect(screen.getByText('rev-active')).toBeInTheDocument();
  expect(screen.getByText('rev-desired')).toBeInTheDocument();
  expect(screen.getByText('["channels"]')).toBeInTheDocument();
});

test('keeps going when reload status stays in progress', async () => {
  api.reloadConfig.mockResolvedValue({ success: true, message: 'still publishing' });
  api.getStatus.mockResolvedValue({ config_reload_in_progress: true, config_reload_status: 'publishing' });
  render(ChangesPage);
  await fireEvent.click(await screen.findByRole('button', { name: 'Reload configuration' }));
  expect(await screen.findByText('publishing')).toBeInTheDocument();
  expect(screen.getByText('true')).toBeInTheDocument();
});
