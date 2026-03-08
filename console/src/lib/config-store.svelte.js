import { api } from './api';
import { GENERAL_SECTION_FIELDS, GENERAL_SECTION_KEY } from './config-nav';
import { t } from './i18n';

export const configStore = $state({
  data: null,
  schema: null,
  status: null,
  loading: false,
  schemaLoading: false,
  loaded: false,
  schemaLoaded: false,
  errorMessage: ''
});

let pendingLoad = null;
let pendingSchemaLoad = null;

function normalizeConfig(response) {
  return typeof response === 'object' && response ? response : {};
}

function cloneValue(value) {
  if (value === undefined) return undefined;
  return JSON.parse(JSON.stringify(value));
}

export async function loadConfigStore({ force = false } = {}) {
  if (pendingLoad) {
    return pendingLoad;
  }

  if (configStore.loaded && !force) {
    return configStore.data;
  }

  configStore.loading = true;

  pendingLoad = (async () => {
    try {
      const [configResponse, statusResponse] = await Promise.all([
        api.getConfig(),
        api.getStatus().catch(() => null)
      ]);

      configStore.data = normalizeConfig(configResponse);
      configStore.status = statusResponse;
      configStore.errorMessage = '';
      configStore.loaded = true;
      return configStore.data;
    } catch (error) {
      configStore.errorMessage = error instanceof Error ? error.message : t('config.loadFailed');
      throw error;
    } finally {
      configStore.loading = false;
      pendingLoad = null;
    }
  })();

  return pendingLoad;
}

export async function loadConfigSchemaStore({ force = false } = {}) {
  if (pendingSchemaLoad) {
    return pendingSchemaLoad;
  }

  if (configStore.schemaLoaded && !force) {
    return configStore.schema;
  }

  configStore.schemaLoading = true;

  pendingSchemaLoad = (async () => {
    try {
      configStore.schema = (await api.getConfigSchema()) ?? {};
      configStore.errorMessage = '';
      configStore.schemaLoaded = true;
      return configStore.schema;
    } catch (error) {
      configStore.errorMessage = error instanceof Error ? error.message : t('config.loadFailed');
      throw error;
    } finally {
      configStore.schemaLoading = false;
      pendingSchemaLoad = null;
    }
  })();

  return pendingSchemaLoad;
}

export async function loadConfigBundle({ force = false } = {}) {
  const [config, schema] = await Promise.all([
    loadConfigStore({ force }),
    loadConfigSchemaStore({ force })
  ]);

  return { config, schema, status: configStore.status };
}

export function readSectionValue(config, sectionKey) {
  const source = normalizeConfig(config);

  if (sectionKey === GENERAL_SECTION_KEY) {
    return Object.fromEntries(
      GENERAL_SECTION_FIELDS.map((fieldKey) => [fieldKey, cloneValue(source[fieldKey]) ?? null])
    );
  }

  return cloneValue(source[sectionKey]) ?? {};
}

export function writeSectionValue(config, sectionKey, sectionValue) {
  const nextConfig = cloneValue(normalizeConfig(config));

  if (sectionKey === GENERAL_SECTION_KEY) {
    for (const fieldKey of GENERAL_SECTION_FIELDS) {
      nextConfig[fieldKey] = cloneValue(sectionValue?.[fieldKey]) ?? null;
    }
    return nextConfig;
  }

  nextConfig[sectionKey] = cloneValue(sectionValue) ?? {};
  return nextConfig;
}

export function buildSectionPayload(sectionKey, sectionValue) {
  if (sectionKey === GENERAL_SECTION_KEY) {
    return Object.fromEntries(
      GENERAL_SECTION_FIELDS.map((fieldKey) => [fieldKey, cloneValue(sectionValue?.[fieldKey]) ?? null])
    );
  }

  return {
    [sectionKey]: cloneValue(sectionValue) ?? {}
  };
}

export function updateConfigStore(nextConfig) {
  configStore.data = normalizeConfig(nextConfig);
  configStore.loaded = true;
  configStore.errorMessage = '';
}
