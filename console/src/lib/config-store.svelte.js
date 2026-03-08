import { api } from './api';

export const configStore = $state({
  data: null,
  status: null,
  loading: false,
  loaded: false,
  errorMessage: ''
});

let pendingLoad = null;

function normalizeConfig(response) {
  return typeof response === 'object' && response ? response : {};
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
      configStore.errorMessage = error instanceof Error ? error.message : 'Failed to load config';
      throw error;
    } finally {
      configStore.loading = false;
      pendingLoad = null;
    }
  })();

  return pendingLoad;
}

export function updateConfigStore(nextConfig) {
  configStore.data = nextConfig;
  configStore.loaded = true;
  configStore.errorMessage = '';
}
