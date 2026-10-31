import { fireEvent, render, screen } from '@testing-library/svelte';
import { beforeEach, expect, test, vi } from 'vitest';

vi.mock('../lib/api', () => ({
  api: {
    getChannelsStatus: vi.fn(),
    sendChannel: vi.fn()
  }
}));

import { api } from '../lib/api';
import ChannelsPage from './ChannelsPage.svelte';

const channel = { name: 'telegram', type: 'telegram', status: 'configured', enabled: true };

beforeEach(() => {
  api.getChannelsStatus.mockReset();
  api.sendChannel.mockReset();
  api.getChannelsStatus.mockResolvedValue({ channels: [channel] });
});

test('labels channel status as configured state', async () => {
  render(ChannelsPage);
  expect(await screen.findByText('Configured state: Configured')).toBeInTheDocument();
  expect(screen.getByText(/not a live health check/)).toBeInTheDocument();
});

test('shows a denied load separately from an empty channel list', async () => {
  const error = new Error('refused');
  error.status = 403;
  api.getChannelsStatus.mockRejectedValue(error);
  render(ChannelsPage);
  const alert = await screen.findByRole('alert');
  expect(alert).toHaveTextContent('refused');
  expect(alert.closest('.ui-state-denied')).toBeTruthy();
});

test('renders only the send response fields', async () => {
  api.sendChannel.mockResolvedValue({ channel: 'telegram', delivered: false });
  render(ChannelsPage);
  await screen.findByText('Configured state: Configured');
  await fireEvent.click(screen.getByRole('button', { name: 'Send' }));
  expect(await screen.findByText('false')).toBeInTheDocument();
  expect(screen.getByText('The response did not include this field.')).toBeInTheDocument();
  expect(api.sendChannel).toHaveBeenCalledOnce();
});

test('shows denial and does not resend', async () => {
  const error = new Error('send refused');
  error.status = 403;
  api.sendChannel.mockRejectedValue(error);
  render(ChannelsPage);
  await screen.findByRole('button', { name: 'Send' });
  await fireEvent.click(screen.getByRole('button', { name: 'Send' }));
  const alert = await screen.findByRole('alert');
  expect(alert).toHaveTextContent('send refused');
  expect(api.sendChannel).toHaveBeenCalledOnce();
});

test('keeps the draft when delivery is uncertain', async () => {
  api.sendChannel.mockRejectedValue(new TypeError('network'));
  render(ChannelsPage);
  const recipient = await screen.findByLabelText('Recipient');
  await fireEvent.input(recipient, { target: { value: 'prx_user' } });
  await fireEvent.click(screen.getByRole('button', { name: 'Send' }));
  expect(await screen.findByText(/was not sent again/)).toBeInTheDocument();
  expect(recipient).toHaveValue('prx_user');
  expect(api.sendChannel).toHaveBeenCalledOnce();
});
