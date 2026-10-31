export const TOKEN_STORAGE_KEY = 'prx-console-token';
export const DEFAULT_API_URL = 'http://localhost:8080';

// Single navigation group table. Later packages append items inside a group.
// Do not add a fifth group from this table.
export const NAV_GROUPS = [
  {
    id: 'work',
    labelKey: 'nav.groups.work',
    items: [
      { labelKey: 'nav.sessions', path: '/sessions', activePrefix: '/chat/' },
      { labelKey: 'decisionInspect.title', path: '/decisions' }
    ]
  },
  {
    id: 'capabilities',
    labelKey: 'nav.groups.capabilities',
    items: [
      { labelKey: 'nav.providers', path: '/providers' },
      { labelKey: 'nav.models', path: '/models' },
      { labelKey: 'nav.routing', path: '/routing' },
      { labelKey: 'nav.channels', path: '/channels' },
      { labelKey: 'nav.skills', path: '/skills' },
      { labelKey: 'nav.plugins', path: '/plugins' },
      { labelKey: 'nav.mcp', path: '/mcp' }
    ]
  },
  {
    id: 'automation',
    labelKey: 'nav.groups.automation',
    items: [
      { labelKey: 'nav.hooks', path: '/hooks' },
      { labelKey: 'decisionPlane.title', path: '/decision-plane' }
    ]
  },
  {
    id: 'operations',
    labelKey: 'nav.groups.operations',
    items: [
      { labelKey: 'nav.overview', path: '/overview' },
      { labelKey: 'nav.config', path: '/config' },
      { labelKey: 'nav.changes', path: '/changes' },
      { labelKey: 'nav.logs', path: '/logs' }
    ]
  }
];

// Append-only map of renamed paths. C1 renames nothing, so every current
// path still resolves where it did. When a path is renamed, set the old
// path to the new path here.
export const ROUTE_REDIRECTS = {};

export function resolveRedirect(pathname) {
  if (typeof pathname !== 'string') {
    return null;
  }
  const target = ROUTE_REDIRECTS[pathname];
  if (typeof target !== 'string' || target.length === 0 || target === pathname) {
    return null;
  }
  return target;
}

export function activeNavTarget(pathname) {
  if (typeof pathname !== 'string' || pathname.length === 0) {
    return null;
  }

  for (const group of NAV_GROUPS) {
    for (const item of group.items) {
      if (pathname === item.path) {
        return { groupId: group.id, path: item.path };
      }
      if (typeof item.activePrefix === 'string' && pathname.startsWith(item.activePrefix)) {
        return { groupId: group.id, path: item.path };
      }
    }
  }

  return null;
}
