import { fireEvent, render, screen } from '@testing-library/svelte';
import { expect, test, vi } from 'vitest';
import Form from './Form.svelte';

function renderForm(validate, onValid = vi.fn()) {
  render(Form, {
    props: {
      fields: [{ name: 'title', label: 'Title' }],
      initialValues: { title: 'draft' },
      submitLabel: 'Save',
      validate,
      onValid
    }
  });
  return { onValid, input: screen.getByLabelText('Title'), form: screen.getByRole('button', { name: 'Save' }).closest('form') };
}

test('keeps the entered value when validation fails', async () => {
  const { onValid, input, form } = renderForm(() => ({ title: 'Too short' }));
  await fireEvent.input(input, { target: { value: 'draft text' } });
  await fireEvent.submit(form);
  expect(input).toHaveValue('draft text');
  expect(screen.getByText('Too short')).toBeInTheDocument();
  expect(screen.getByText('Correct the highlighted fields. Your entries were kept.')).toBeInTheDocument();
  expect(onValid).not.toHaveBeenCalled();
});

test('does not submit while a composition is open', async () => {
  const { onValid, input, form } = renderForm(() => ({}));
  await fireEvent.compositionStart(input);
  await fireEvent.submit(form);
  expect(input).toHaveValue('draft');
  expect(onValid).not.toHaveBeenCalled();
  await fireEvent.compositionEnd(input);
  await fireEvent.submit(form);
  expect(onValid).toHaveBeenCalledWith({ title: 'draft' });
});
