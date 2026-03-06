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
  getSessions: () => request('/api/sessions'),
  getSessionMessages: (sessionId) =>
    request(`/api/sessions/${encodeURIComponent(sessionId)}/messages`),
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
    const token = getToken();
    if (token) {
      params.set('token', token);
    }
    return `${apiBaseUrl}/api/sessions/media?${params.toString()}`;
  },
  getChannelsStatus: () => request('/api/channels/status'),
  getConfig: () => request('/api/config'),
  saveConfig: (data) =>
    request('/api/config', {
      method: 'POST',
      body: JSON.stringify(data)
    }),
  getHooks: () => request('/api/hooks'),
  getMcpServers: () => request('/api/mcp/servers'),
  getSkills: () => request('/api/skills')
};

export { ApiError, apiBaseUrl };
