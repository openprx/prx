import { fireEvent, render, screen } from '@testing-library/svelte';
import { expect, test, vi } from 'vitest';
import Field from './Field.svelte';

test('associates the label and reports a validation error', async () => {
  const onValue = vi.fn();
  render(Field, {
    props: {
      name: 'title',
      label: 'Title',
      value: 'draft',
      error: 'Too short',
      onValue
    }
  });

  const input = screen.getByLabelText('Title');
  expect(input).toHaveValue('draft');
  expect(input).toHaveAttribute('aria-invalid', 'true');
  expect(screen.getByText('Too short')).toBeInTheDocument();

  await fireEvent.input(input, { target: { value: 'draft text' } });
  expect(onValue).toHaveBeenCalledWith('draft text');
});
