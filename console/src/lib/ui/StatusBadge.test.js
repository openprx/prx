import { render, screen } from '@testing-library/svelte';
import { expect, test } from 'vitest';
import StatusBadge from './StatusBadge.svelte';

test('renders the status label and tone', () => {
  render(StatusBadge, { props: { label: 'Connected', tone: 'success' } });
  const badge = screen.getByText('Connected');
  expect(badge).toHaveClass('ui-badge-success');
});

test('shows unknown when the label is blank', () => {
  render(StatusBadge, { props: { label: '   ', tone: 'neutral' } });
  expect(screen.getByText('Unknown')).toBeInTheDocument();
});
