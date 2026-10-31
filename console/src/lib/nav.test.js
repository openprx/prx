import { expect, test } from 'vitest';
import { NAV_GROUPS, ROUTE_REDIRECTS, activeNavTarget, resolveRedirect } from './constants.js';

const CURRENT_PATHS = [
  '/overview',
  '/sessions',
  '/activity',
  '/approvals',
  '/terminals',
  '/decision-plane',
  '/decisions',
  '/channels',
  '/hooks',
  '/mcp',
  '/skills',
  '/plugins',
  '/providers',
  '/models',
  '/routing',
  '/changes',
  '/config',
  '/logs',
  '/schedules',
  '/memory',
  '/cost'
];

test('group table covers every current nav path once and chat counts as sessions', () => {
  const grouped = NAV_GROUPS.flatMap((group) => group.items.map((item) => item.path));
  expect([...grouped].sort()).toEqual([...CURRENT_PATHS].sort());
  expect(NAV_GROUPS.map((group) => group.id)).toEqual([
    'work',
    'capabilities',
    'automation',
    'operations'
  ]);

  for (const path of CURRENT_PATHS) {
    expect(grouped.filter((item) => item === path)).toHaveLength(1);
    expect(ROUTE_REDIRECTS[path]).toBeUndefined();
    expect(resolveRedirect(path)).toBeNull();
  }

  expect(activeNavTarget('/chat/example')).toEqual({ groupId: 'work', path: '/sessions' });
  expect(NAV_GROUPS.find((group) => group.id === 'capabilities').items.map((item) => item.path)).toEqual([
    '/providers',
    '/models',
    '/routing',
    '/channels',
    '/skills',
    '/plugins',
    '/mcp',
    '/config',
    '/changes'
  ]);
});
