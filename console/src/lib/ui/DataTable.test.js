import { render, screen } from '@testing-library/svelte';
import { expect, test } from 'vitest';
import DataTable from './DataTable.svelte';

const longId = 'session-0123456789abcdef0123456789abcdef';

function mockClipMetrics() {
  const scroll = Object.getOwnPropertyDescriptor(Element.prototype, 'scrollWidth');
  const client = Object.getOwnPropertyDescriptor(Element.prototype, 'clientWidth');
  Object.defineProperty(Element.prototype, 'scrollWidth', {
    configurable: true,
    get() {
      return (this.textContent ?? '').length > 24 ? 480 : 40;
    }
  });
  Object.defineProperty(Element.prototype, 'clientWidth', {
    configurable: true,
    get() {
      return 160;
    }
  });
  return () => {
    if (scroll) {
      Object.defineProperty(Element.prototype, 'scrollWidth', scroll);
    }
    if (client) {
      Object.defineProperty(Element.prototype, 'clientWidth', client);
    }
  };
}

test('renders a semantic table and focuses a clipped identifier', () => {
  const restore = mockClipMetrics();
  try {
    render(DataTable, {
      props: {
        caption: 'Sessions',
        columns: [
          { key: 'id', label: 'Session', truncate: true },
          { key: 'sender', label: 'Sender' }
        ],
        rows: [{ id: longId, sender: 'operator' }]
      }
    });

    expect(screen.getByRole('table', { name: 'Sessions' })).toBeInTheDocument();
    expect(screen.getByRole('columnheader', { name: 'Session' })).toHaveClass('ui-data-table-head');
    const identifiers = screen.getAllByText(longId);
    expect(identifiers.length).toBeGreaterThan(0);
    expect(identifiers[0].tagName).toBe('SPAN');
    expect(identifiers[0]).toHaveClass('ui-truncate-id');
    expect(identifiers[0]).toHaveAttribute('title', longId);
    expect(identifiers[0]).toHaveAttribute('tabindex', '0');
    identifiers[0].focus();
    expect(identifiers[0]).toHaveFocus();
    expect(screen.getAllByText('operator').length).toBeGreaterThan(0);
  } finally {
    restore();
  }
});

test('leaves a short identifier out of the tab order', () => {
  const restore = mockClipMetrics();
  try {
    render(DataTable, {
      props: {
        columns: [{ key: 'id', label: 'Session', truncate: true }],
        rows: [{ id: 'short' }]
      }
    });

    const identifiers = screen.getAllByText('short');
    expect(identifiers[0].tagName).toBe('SPAN');
    expect(identifiers[0]).not.toHaveAttribute('tabindex');
  } finally {
    restore();
  }
});

test('accepts duplicate row ids', () => {
  render(DataTable, {
    props: {
      columns: [{ key: 'id', label: 'Session' }],
      rows: [{ id: 'same' }, { id: 'same' }]
    }
  });
  expect(screen.getAllByText('same').length).toBeGreaterThan(1);
});

test('shows the empty state when there are no rows', () => {
  render(DataTable, {
    props: {
      columns: [{ key: 'id', label: 'Session' }],
      rows: []
    }
  });
  expect(screen.getByRole('status')).toHaveTextContent('No rows to show.');
});
