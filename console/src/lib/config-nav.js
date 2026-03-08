export const GENERAL_SECTION_KEY = 'general';

export const GENERAL_SECTION_FIELDS = [
  'api_key',
  'api_url',
  'default_provider',
  'default_model',
  'default_temperature'
];

const SECTION_PRIORITY = [
  GENERAL_SECTION_KEY,
  'agent',
  'memory',
  'channels_config',
  'security',
  'gateway',
  'runtime',
  'observability',
  'reliability',
  'scheduler',
  'heartbeat',
  'skills',
  'mcp',
  'browser',
  'http_request',
  'web_search',
  'proxy',
  'cost',
  'storage',
  'tunnel',
  'identity',
  'media',
  'hardware',
  'peripherals',
  'nodes',
  'agents',
  'auth',
  'secrets',
  'composio',
  'webhook',
  'cron',
  'sessions_spawn',
  'query_classification',
  'model_routes',
  'embedding_routes',
  'skill_rag',
  'self_system',
  'user_policies',
  'identity_bindings',
  'multimodal'
];

const SECTION_META = {
  general: { labelKey: 'config.section.general', fallbackLabel: 'General' },
  agent: { labelKey: 'config.section.agent', fallbackLabel: 'Agent' },
  memory: { labelKey: 'config.section.memory', fallbackLabel: 'Memory' },
  channels_config: { labelKey: 'config.section.channels', fallbackLabel: 'Channels' },
  security: { labelKey: 'config.section.security', fallbackLabel: 'Security' },
  gateway: { labelKey: 'config.section.gateway', fallbackLabel: 'Gateway' },
  runtime: { labelKey: 'config.section.runtime', fallbackLabel: 'Runtime' },
  observability: { labelKey: 'config.section.observability', fallbackLabel: 'Observability' },
  reliability: { labelKey: 'config.section.reliability', fallbackLabel: 'Reliability' },
  scheduler: { labelKey: 'config.section.scheduler', fallbackLabel: 'Scheduler' },
  heartbeat: { labelKey: 'config.section.heartbeat', fallbackLabel: 'Heartbeat' },
  skills: { labelKey: 'config.section.skills', fallbackLabel: 'Skills' },
  mcp: { labelKey: 'config.section.mcp', fallbackLabel: 'MCP' },
  browser: { labelKey: 'config.section.browser', fallbackLabel: 'Browser' },
  http_request: { labelKey: 'config.section.httpRequest', fallbackLabel: 'HTTP Request' },
  web_search: { labelKey: 'config.section.webSearch', fallbackLabel: 'Web Search' },
  proxy: { labelKey: 'config.section.proxy', fallbackLabel: 'Proxy' },
  cost: { labelKey: 'config.section.cost', fallbackLabel: 'Cost' },
  storage: { labelKey: 'config.section.storage', fallbackLabel: 'Storage' },
  tunnel: { labelKey: 'config.section.tunnel', fallbackLabel: 'Tunnel' },
  identity: { labelKey: 'config.section.identity', fallbackLabel: 'Identity' },
  media: { labelKey: 'config.section.media', fallbackLabel: 'Media' },
  hardware: { labelKey: 'config.section.hardware', fallbackLabel: 'Hardware' }
};

export function humanizeKey(key) {
  return String(key)
    .replace(/_/g, ' ')
    .replace(/\b\w/g, (character) => character.toUpperCase());
}

function getKnownSectionKeys() {
  return new Set(SECTION_PRIORITY.filter((key) => key !== GENERAL_SECTION_KEY));
}

function getConfigRootKeys(config) {
  if (!config || typeof config !== 'object') {
    return [];
  }

  return Object.keys(config).filter((key) => !GENERAL_SECTION_FIELDS.includes(key));
}

function sortSectionKeys(keys) {
  const priorityMap = new Map(SECTION_PRIORITY.map((key, index) => [key, index]));
  return [...keys].sort((left, right) => {
    const leftPriority = priorityMap.get(left) ?? Number.MAX_SAFE_INTEGER;
    const rightPriority = priorityMap.get(right) ?? Number.MAX_SAFE_INTEGER;
    if (leftPriority !== rightPriority) {
      return leftPriority - rightPriority;
    }
    return left.localeCompare(right);
  });
}

export function getConfigSectionMeta(sectionKey) {
  const meta = SECTION_META[sectionKey];
  return {
    groupKey: sectionKey,
    labelKey: meta?.labelKey ?? null,
    fallbackLabel: meta?.fallbackLabel ?? humanizeKey(sectionKey)
  };
}

export function buildConfigNavGroups(config) {
  const sectionKeys = new Set(getKnownSectionKeys());
  for (const key of getConfigRootKeys(config)) {
    sectionKeys.add(key);
  }

  return [GENERAL_SECTION_KEY, ...sortSectionKeys(sectionKeys)]
    .filter((groupKey, index, items) => items.indexOf(groupKey) === index)
    .map((groupKey) => getConfigSectionMeta(groupKey));
}

export function configSectionId(groupKey) {
  return `config-section-${groupKey}`;
}

export function normalizeConfigSectionHash(hashValue = '') {
  const hash = String(hashValue).replace(/^#/, '').trim();
  if (!hash) return '';
  if (hash.startsWith('config-section-')) {
    return hash.slice('config-section-'.length);
  }
  return hash;
}

export function configSectionHash(groupKey) {
  return `#${groupKey}`;
}

export function focusConfigSection(groupKey) {
  if (typeof document === 'undefined' || typeof window === 'undefined' || !groupKey) return;

  const target = document.getElementById(configSectionId(groupKey));
  if (target) {
    target.scrollIntoView({ behavior: 'smooth', block: 'start' });
  }

  const nextHash = configSectionHash(groupKey);
  if (window.location.hash !== nextHash) {
    window.location.hash = nextHash;
  }
}
