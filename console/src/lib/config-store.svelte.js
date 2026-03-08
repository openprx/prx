import { api } from './api';

export const configStore = $state({
  data: null,
  status: null,
  loading: false,
  loaded: false,
  errorMessage: ''
});

function normalizeConfig(response) {
  return typeof response === 'object' && response ? response : {};
}

export async function loadConfigStore({ force = false } = {}) {
  if (configStore.loading || (configStore.loaded && !force)) {
    return configStore.data;
  }

  configStore.loading = true;

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
  }
}

export function updateConfigStore(nextConfig) {
  configStore.data = nextConfig;
  configStore.loaded = true;
  configStore.errorMessage = '';
}
