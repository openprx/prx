import { render, screen } from '@testing-library/svelte';
import { expect, test } from 'vitest';
import ErrorNotice from './ErrorNotice.svelte';

test('renders the message and exposes role alert', () => {
  render(ErrorNotice, { props: { error: new Error('gateway refused the request') } });
  const alert = screen.getByRole('alert');
  expect(alert).toHaveTextContent('gateway refused the request');
});
