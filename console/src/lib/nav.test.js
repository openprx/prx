import { expect, test } from 'vitest';
import { NAV_GROUPS, NAV_ITEMS, ROUTE_REDIRECTS, activeNavTarget, resolveRedirect } from './constants.js';

const CURRENT_PATHS = [
  '/overview',
  '/sessions',
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
  '/logs'
];

test('group table covers every current nav path once and chat counts as sessions', () => {
  const grouped = NAV_GROUPS.flatMap((group) => group.items.map((item) => item.path));
  expect(NAV_ITEMS.map((item) => item.path).sort()).toEqual([...CURRENT_PATHS].sort());
  expect(NAV_GROUPS.map((group) => group.id)).toEqual([
    'work',
    'capabilities',
    'automation',
    'operations'
  ]);

  for (const item of NAV_ITEMS) {
    expect(grouped.filter((path) => path === item.path)).toHaveLength(1);
    expect(ROUTE_REDIRECTS[item.path]).toBeUndefined();
    expect(resolveRedirect(item.path)).toBeNull();
  }

  expect(activeNavTarget('/chat/example')).toEqual({ groupId: 'work', path: '/sessions' });
});
