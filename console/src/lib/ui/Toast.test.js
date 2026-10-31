import { fireEvent, render, screen } from '@testing-library/svelte';
import { expect, test, vi } from 'vitest';
import Toast from './Toast.svelte';

test('announces the message in a polite live region', async () => {
  const onDismiss = vi.fn();
  render(Toast, { props: { message: 'Saved.', onDismiss } });
  const region = screen.getByRole('status');
  expect(region).toHaveAttribute('aria-live', 'polite');
  expect(region).toHaveTextContent('Saved.');
  await fireEvent.click(screen.getByRole('button', { name: 'Dismiss notification' }));
  expect(onDismiss).toHaveBeenCalledOnce();
});
