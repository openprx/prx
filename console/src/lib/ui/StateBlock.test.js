import { render, screen } from '@testing-library/svelte';
import { expect, test } from 'vitest';
import StateBlock from './StateBlock.svelte';

test('renders each shared state with an accessible role', () => {
  const cases = [
    ['loading', 'status', 'Loading sessions'],
    ['empty', 'status', 'No sessions'],
    ['error', 'alert', 'Could not load sessions'],
    ['denied', 'alert', 'You do not have permission to view this.'],
    ['unavailable', 'status', 'Unavailable in this deployment.'],
    ['disconnected-stale', 'status', 'Connection lost or the data is out of date.']
  ];

  for (const [variant, role, message] of cases) {
    const view = render(StateBlock, { props: { variant, message } });
    const node = screen.getByRole(role);
    expect(node).toHaveTextContent(message);
    view.unmount();
  }
});

test('uses the unavailable copy when no message is given', () => {
  render(StateBlock, { props: { variant: 'unavailable' } });
  expect(screen.getByRole('status')).toHaveTextContent('Unavailable in this deployment.');
});
