import { clearToken, getToken } from './auth';
import { navigate } from './router';

const configuredUrl = (import.meta.env.VITE_API_URL ?? '').trim();
const apiBaseUrl = configuredUrl.endsWith('/') ? configuredUrl.slice(0, -1) : configuredUrl;

class ApiError extends Error {
  constructor(status, message) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
  }
}

async function parseResponseBody(response) {
  const contentType = response.headers.get('content-type') || '';
  if (contentType.includes('application/json')) {
    return response.json().catch(() => null);
  }

  return response.text().catch(() => null);
}

function resolveErrorMessage(body, status) {
  if (body && typeof body === 'object' && typeof body.error === 'string') {
    return body.error;
  }

  return `Request failed (${status})`;
}

async function request(path, init = {}) {
  const token = getToken();
  const headers = {
    Accept: 'application/json',
    ...init.headers
  };

  if (token) {
    headers.Authorization = `Bearer ${token}`;
  }

  if (init.body && !(init.body instanceof FormData) && !headers['Content-Type']) {
    headers['Content-Type'] = 'application/json';
  }

  const response = await fetch(`${apiBaseUrl}${path}`, {
    ...init,
    credentials: init.credentials ?? 'include',
    headers
  });

  const body = await parseResponseBody(response);

  if (response.status === 401) {
    clearToken();
    navigate('/', true);
    throw new ApiError(401, 'Unauthorized');
  }

  if (!response.ok) {
    throw new ApiError(response.status, resolveErrorMessage(body, response.status));
  }

  return body;
}

export const api = {
  getStatus: () => request('/api/status'),
  getSessions: ({ limit, offset, channel, status, search } = {}) => {
    const params = new URLSearchParams();
    if (limit) params.set('limit', String(limit));
    if (offset) params.set('offset', String(offset));
    if (channel) params.set('channel', channel);
    if (status) params.set('status', status);
    if (search) params.set('search', search);
    const suffix = params.size > 0 ? `?${params.toString()}` : '';
    return request(`/api/sessions${suffix}`);
  },
  getSessionMessages: (sessionId, { limit, offset } = {}) => {
    const params = new URLSearchParams();
    if (limit) params.set('limit', String(limit));
    if (offset) params.set('offset', String(offset));
    const suffix = params.size > 0 ? `?${params.toString()}` : '';
    return request(`/api/sessions/${encodeURIComponent(sessionId)}/messages${suffix}`);
  },
  sendMessage: (sessionId, message) =>
    request(`/api/sessions/${encodeURIComponent(sessionId)}/message`, {
      method: 'POST',
      body: JSON.stringify({ message })
    }),
  sendMessageWithMedia: (sessionId, message, files = []) => {
    if (!Array.isArray(files) || files.length === 0) {
      return api.sendMessage(sessionId, message);
    }

    const formData = new FormData();
    formData.append('message', message);
    for (const file of files) {
      formData.append('files', file);
    }

    return request(`/api/sessions/${encodeURIComponent(sessionId)}/message`, {
      method: 'POST',
      body: formData
    });
  },
  getSessionMediaUrl: (path) => {
    const params = new URLSearchParams({ path });
    return `${apiBaseUrl}/api/sessions/media?${params.toString()}`;
  },
  getChannelsStatus: () => request('/api/channels/status'),
  getConfig: () => request('/api/config'),
  getConfigSchema: () => request('/api/config/schema'),
  getConfigFiles: () => request('/api/config/files'),
  saveConfig: (data) =>
    request('/api/config', {
      method: 'POST',
      body: JSON.stringify(data)
    }),
  saveConfigFile: (filename, content) =>
    request(`/api/config/files/${encodeURIComponent(filename)}`, {
      method: 'PUT',
      body: JSON.stringify({ content })
    }),
  getHooks: () => request('/api/hooks'),
  createHook: (data) =>
    request('/api/hooks', {
      method: 'POST',
      body: JSON.stringify(data)
    }),
  updateHook: (id, data) =>
    request(`/api/hooks/${encodeURIComponent(id)}`, {
      method: 'PUT',
      body: JSON.stringify(data)
    }),
  deleteHook: (id) =>
    request(`/api/hooks/${encodeURIComponent(id)}`, {
      method: 'DELETE'
    }),
  toggleHook: (id) =>
    request(`/api/hooks/${encodeURIComponent(id)}/toggle`, {
      method: 'PATCH'
    }),
  getMcpServers: () => request('/api/mcp/servers'),
  getSkills: () => request('/api/skills'),
  discoverSkills: (source = 'github', query = '') => {
    const params = new URLSearchParams();
    if (source) params.set('source', source);
    if (query) params.set('query', query);
    return request(`/api/skills/discover?${params.toString()}`);
  },
  installSkill: (url, name) =>
    request('/api/skills/install', {
      method: 'POST',
      body: JSON.stringify({ url, name })
    }),
  uninstallSkill: (name) =>
    request(`/api/skills/${encodeURIComponent(name)}`, {
      method: 'DELETE'
    }),
  toggleSkill: (id) =>
    request(`/api/skills/${encodeURIComponent(id)}/toggle`, {
      method: 'PATCH'
    }),
  getPlugins: () => request('/api/plugins'),
  reloadPlugin: (name) =>
    request(`/api/plugins/${encodeURIComponent(name)}/reload`, {
      method: 'POST'
    })
};

export { ApiError, apiBaseUrl };
