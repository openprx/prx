import { render, screen } from '@testing-library/svelte';
import { expect, test } from 'vitest';
import PageHeader from './PageHeader.svelte';

test('renders the page title as a heading', () => {
  render(PageHeader, { props: { title: 'Overview' } });
  expect(screen.getByRole('heading', { name: 'Overview' })).toBeInTheDocument();
});

test('renders an optional description', () => {
  render(PageHeader, { props: { title: 'Sessions', description: 'Saved conversations' } });
  expect(screen.getByText('Saved conversations')).toBeInTheDocument();
});
