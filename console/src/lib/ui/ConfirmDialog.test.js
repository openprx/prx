import { fireEvent, render, screen } from '@testing-library/svelte';
import { tick } from 'svelte';
import { expect, test, vi } from 'vitest';
import ConfirmDialog from './ConfirmDialog.svelte';

const props = {
  open: true,
  title: 'Remove hook',
  objectName: 'hook:backup',
  consequence: 'This removes the hook.',
  confirmLabel: 'Confirm',
  cancelLabel: 'Cancel'
};

test('uses a distinct id for each open dialog', async () => {
  render(ConfirmDialog, { props });
  render(ConfirmDialog, { props: { ...props, objectName: 'hook:other' } });
  await tick();

  const dialogs = screen.getAllByRole('dialog');
  expect(dialogs).toHaveLength(2);
  expect(dialogs[0].getAttribute('aria-labelledby')).not.toBe(dialogs[1].getAttribute('aria-labelledby'));
  expect(dialogs[0].getAttribute('aria-describedby')).not.toBe(dialogs[1].getAttribute('aria-describedby'));
});

test('names the object and the consequence', async () => {
  render(ConfirmDialog, { props });
  await tick();
  const dialog = screen.getByRole('dialog');
  expect(dialog).toHaveTextContent('hook:backup');
  expect(dialog).toHaveTextContent('This removes the hook.');
});

test('closes from escape and traps tab at the ends', async () => {
  const onCancel = vi.fn();
  render(ConfirmDialog, { props: { ...props, onCancel } });
  await tick();

  await fireEvent.keyDown(document, { key: 'Escape' });
  expect(onCancel).toHaveBeenCalledOnce();

  const cancel = screen.getByRole('button', { name: 'Cancel' });
  const confirm = screen.getByRole('button', { name: 'Confirm' });
  confirm.focus();
  await fireEvent.keyDown(document, { key: 'Tab' });
  expect(document.activeElement).toBe(cancel);

  await fireEvent.keyDown(document, { key: 'Tab', shiftKey: true });
  expect(document.activeElement).toBe(confirm);
});

test('restores focus when the dialog closes', async () => {
  const anchor = document.createElement('button');
  anchor.textContent = 'Anchor';
  document.body.appendChild(anchor);
  anchor.focus();

  const view = render(ConfirmDialog, { props });
  await tick();
  expect(document.activeElement).not.toBe(anchor);

  await view.rerender({ ...props, open: false });
  await tick();
  expect(document.activeElement).toBe(anchor);
  anchor.remove();
});
