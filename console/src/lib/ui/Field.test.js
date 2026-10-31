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

test('gives each instance its own id when the name matches', () => {
  render(Field, { props: { name: 'title', label: 'Title A', value: 'a' } });
  render(Field, { props: { name: 'title', label: 'Title B', value: 'b' } });

  const first = screen.getByLabelText('Title A');
  const second = screen.getByLabelText('Title B');
  expect(first.id).not.toBe(second.id);
  expect(first.id.startsWith('field-title-')).toBe(true);
  expect(second.id.startsWith('field-title-')).toBe(true);
});
