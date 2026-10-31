import { render, screen } from '@testing-library/svelte';
import { expect, test } from 'vitest';
import { presentContext } from '../context-status.js';
import ContextHeader from './ContextHeader.svelte';

test('renders unknown for every missing status field', () => {
  const facts = presentContext(null, '');
  expect(facts.length).toBeGreaterThan(0);
  for (const fact of facts) {
    expect(fact.value.trim().length).toBeGreaterThan(0);
    expect(fact.value).toBe('Unknown');
  }

  render(ContextHeader, { props: { status: null, origin: '' } });
  expect(screen.getByRole('region', { name: 'Gateway' })).toBeInTheDocument();
  expect(screen.getAllByText('Unknown').length).toBe(facts.length);
});

test('renders gateway origin, revisions, restart, and provider degradation', () => {
  const status = {
    version: '1.2.3',
    active_config_generation: 4,
    active_config_source_revision: 'abc123',
    desired_config_source_revision: 'def456',
    config_reload_status: 'applied',
    config_reload_in_progress: false,
    restart_required: ['gateway'],
    provider_degraded: true
  };
  const facts = Object.fromEntries(presentContext(status, 'http://127.0.0.1:8080').map((fact) => [fact.id, fact.value]));
  expect(facts.origin).toBe('http://127.0.0.1:8080');
  expect(facts.version).toBe('1.2.3');
  expect(facts.generation).toBe('4');
  expect(facts.activeRevision).toBe('abc123');
  expect(facts.desiredRevision).toBe('def456');
  expect(facts.reload).toBe('Applied');
  expect(facts.restart).toBe('Required: gateway');
  expect(facts.degradation).toBe('Degraded');
  expect(Object.values(facts).every((value) => value.trim().length > 0)).toBe(true);
});
