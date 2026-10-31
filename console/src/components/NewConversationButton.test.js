import { fireEvent, render, screen } from '@testing-library/svelte';
import { beforeEach, expect, test, vi } from 'vitest';
import NewConversationButton from './NewConversationButton.svelte';
import { api } from '../lib/api';
import { resetSessionCreateState } from '../lib/new-conversation.svelte.js';

vi.mock('../lib/api', () => ({
  ApiError: class ApiError extends Error {
    constructor(status, message) {
      super(message);
      this.name = 'ApiError';
      this.status = status;
    }
  },
  api: {
    createSession: vi.fn()
  }
}));

beforeEach(() => {
  resetSessionCreateState();
  api.createSession.mockReset();
});

test('one click posts once and a second click waits on the same request', async () => {
  let resolveCreate;
  api.createSession.mockImplementation(() => new Promise((resolve) => {
    resolveCreate = resolve;
  }));
  render(NewConversationButton, { props: { placement: 'header' } });
  const button = screen.getByRole('button', { name: 'New conversation' });
  await fireEvent.click(button);
  await fireEvent.click(button);
  expect(api.createSession).toHaveBeenCalledTimes(1);
  expect(screen.getByRole('button', { name: 'Creating conversation' })).toBeDisabled();
  resolveCreate({ status: 201, body: { session_id: 'session-1' } });
});

test('a denied create shows the error once, on the header', async () => {
  const { ApiError } = await import('../lib/api');
  api.createSession.mockRejectedValue(new ApiError(403, 'denied'));
  render(NewConversationButton, { props: { placement: 'header' } });
  render(NewConversationButton, { props: { placement: 'empty' } });
  const buttons = screen.getAllByRole('button', { name: 'New conversation' });
  await fireEvent.click(buttons[1]);
  expect(await screen.findByRole('alert')).toHaveTextContent('denied');
  expect(screen.getAllByRole('alert')).toHaveLength(1);
  expect(screen.queryByRole('button', { name: 'Refresh list' })).not.toBeInTheDocument();
});

test('an uncertain create offers refresh once and does not post again', async () => {
  api.createSession.mockRejectedValue(new TypeError('failed'));
  render(NewConversationButton, { props: { placement: 'header' } });
  render(NewConversationButton, { props: { placement: 'sessions' } });
  render(NewConversationButton, { props: { placement: 'empty' } });
  const buttons = screen.getAllByRole('button', { name: 'New conversation' });
  await fireEvent.click(buttons[2]);
  expect(await screen.findByRole('status')).toHaveTextContent('uncertain');
  expect(screen.getAllByRole('status')).toHaveLength(1);
  await fireEvent.click(screen.getByRole('button', { name: 'Refresh list' }));
  expect(api.createSession).toHaveBeenCalledTimes(1);
});
