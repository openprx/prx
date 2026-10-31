import { fireEvent, render, screen } from '@testing-library/svelte';
import { beforeEach, expect, test, vi } from 'vitest';

vi.mock('../lib/api', () => ({
  api: {
    getPlugins: vi.fn(),
    reloadPlugin: vi.fn(),
    enablePlugin: vi.fn(),
    disablePlugin: vi.fn(),
    refreshPlugins: vi.fn(),
    installPlugin: vi.fn(),
    updatePlugin: vi.fn(),
    removePlugin: vi.fn()
  }
}));

import { api } from '../lib/api';
import PluginsPage from './PluginsPage.svelte';

const loaded = {
  name: 'voice-talk-realtime',
  version: '1',
  description: 'fixture',
  capabilities: ['tool'],
  status: 'Active',
  permissions_required: ['log'],
  permissions_granted: ['log']
};

beforeEach(() => {
  api.getPlugins.mockReset();
  api.enablePlugin.mockReset();
  api.removePlugin.mockReset();
  api.getPlugins.mockResolvedValue({ plugins: [loaded], count: 1, generation: 3 });
});

test('shows registry facts and the contract gap', async () => {
  render(PluginsPage);
  expect(await screen.findByText('Registry status')).toBeInTheDocument();
  expect(screen.getByText('Active')).toBeInTheDocument();
  expect(screen.getByText(/does not report healthy or listening/)).toBeInTheDocument();
  expect(screen.getByText(/not first-discoverable by reload/)).toBeInTheDocument();
  expect(screen.getByText('Present in the loaded list')).toBeInTheDocument();
});

test('enables a named disabled plugin and shows the returned JSON', async () => {
  api.enablePlugin.mockResolvedValue({ plugin: { name: 'voice-talk-realtime' }, enabled: true, generation: 4 });
  render(PluginsPage);
  const name = await screen.findByLabelText('Disabled plugin name');
  await fireEvent.input(name, { target: { value: 'voice-talk-realtime' } });
  await fireEvent.click(screen.getByRole('button', { name: 'Enable' }));
  expect(await screen.findByText(/"enabled": true/)).toBeInTheDocument();
  expect(api.enablePlugin).toHaveBeenCalledWith('voice-talk-realtime');
});

test('shows a conflict when enable is already done', async () => {
  const error = new Error('plugin is already enabled');
  error.status = 409;
  api.enablePlugin.mockRejectedValue(error);
  render(PluginsPage);
  const name = await screen.findByLabelText('Disabled plugin name');
  await fireEvent.input(name, { target: { value: 'voice-talk-realtime' } });
  await fireEvent.click(screen.getByRole('button', { name: 'Enable' }));
  expect(await screen.findByText('plugin is already enabled')).toBeInTheDocument();
});

test('names the plugin and the trust consequence before removal', async () => {
  render(PluginsPage);
  await fireEvent.click(await screen.findByRole('button', { name: 'Remove' }));
  const dialog = await screen.findByRole('dialog');
  expect(dialog).toHaveTextContent('voice-talk-realtime');
  expect(dialog).toHaveTextContent(/revokes its trust record/);
  expect(api.removePlugin).not.toHaveBeenCalled();
});

test('shows the feature-off note as unavailable', async () => {
  api.getPlugins.mockResolvedValue({
    plugins: [],
    count: 0,
    note: 'WASM plugin system not enabled (compile with --features wasm-plugins)'
  });
  render(PluginsPage);
  expect(await screen.findByText(/WASM plugin system not enabled/)).toBeInTheDocument();
  expect(screen.queryByLabelText('Disabled plugin name')).toBeNull();
});
