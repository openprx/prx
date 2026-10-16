import { clearToken, getToken } from './auth';
import { t } from './i18n';
import { navigate } from './router';

const configuredUrl = (import.meta.env.VITE_API_URL ?? '').trim();
const apiBaseUrl = configuredUrl.endsWith('/') ? configuredUrl.slice(0, -1) : configuredUrl;

class ApiError extends Error {
  constructor(status, message, body = null) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    // The parsed error body, for callers that act on its fields (a 428
    // names the revision its refused plan was computed against).
    this.body = body && typeof body === 'object' ? body : null;
    // Why the gateway refused, beyond the error itself: a 403 at
    // `supervised` autonomy says a Medium or High write needs an approval
    // grant a Gateway request cannot carry.
    this.reason = typeof this.body?.reason === 'string' ? this.body.reason : '';
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

  return t('common.requestFailed', { status });
}

async function request(path, init = {}, { withEtag = false } = {}) {
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
    throw new ApiError(401, t('common.unauthorized'));
  }

  if (!response.ok) {
    throw new ApiError(response.status, resolveErrorMessage(body, response.status), body);
  }

  if (withEtag) {
    return { body, etag: response.headers.get('etag') };
  }
  return body;
}

// A write bound to a revision: `If-Match` names it (quoted), so the write
// lands only on exactly that revision (409 otherwise).
function revisionHeaders(revision) {
  return revision ? { 'If-Match': `"${revision}"` } : {};
}

// `?confirm_high_risk=true` confirms a change the server classifies high
// risk; it counts only together with `If-Match` naming the revision the
// refusing 428 reported (D-049 B).
function confirmSuffix(confirmHighRisk) {
  return confirmHighRisk ? '?confirm_high_risk=true' : '';
}

// The query of a management read. Only the parameters the management
// endpoints take are ever sent (the gateway refuses any other, so no
// cache-busting parameter either).
function managementQuery(entries) {
  const params = new URLSearchParams();
  for (const [key, value] of entries) {
    if (value !== undefined && value !== null && value !== '' && value !== false) {
      params.set(key, String(value));
    }
  }
  return params.size > 0 ? `?${params.toString()}` : '';
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
  testDecisionBackend: ({ backend = 'configured', live = false, confirmHighRisk = false, revision = null } = {}) =>
    request(`/api/causal/test${confirmSuffix(confirmHighRisk)}`, {
      method: 'POST', headers: revisionHeaders(revision), body: JSON.stringify({ backend, live })
    }),
  storeDecisionCredential: (value, revision) => request('/api/causal/credential', {
    method: 'PUT', headers: revisionHeaders(revision), body: JSON.stringify({ value })
  }),
  getDecisionReport: ({ from, to }) =>
    request(`/api/causal/reports?${new URLSearchParams({ from, to })}`),
  getSavedDecisionReports: () => request('/api/causal/reports/saved'),
  getDecisionStatus: () => request('/api/causal/status'),
  planDecisionChange: (mutations) => request('/api/causal/config/plans', {
    method: 'POST', body: JSON.stringify({ mutations })
  }),
  applyDecisionChange: (mutations, { confirmHighRisk = false, revision = null } = {}) =>
    request(`/api/causal/config/apply${confirmSuffix(confirmHighRisk)}`, {
      method: 'POST', headers: revisionHeaders(revision), body: JSON.stringify({ mutations })
    }),
  getDecisions: ({ from, to, limit = 100 }) =>
    request(`/api/causal/decisions?${new URLSearchParams({ from, to, limit: String(limit) })}`),
  getDecision: (id) => request(`/api/causal/decisions/${encodeURIComponent(id)}`),
  getDecisionApproval: (sessionId, decisionId) =>
    request(`/api/sessions/${encodeURIComponent(sessionId)}/approvals/${encodeURIComponent(decisionId)}`),
  reviewDecisionApproval: (sessionId, decisionId, approved) =>
    request(`/api/sessions/${encodeURIComponent(sessionId)}/approvals/${encodeURIComponent(decisionId)}`, {
      method: 'POST', body: JSON.stringify({ approved })
    }),
  revokeDecisionApproval: (sessionId, decisionId) =>
    request(`/api/sessions/${encodeURIComponent(sessionId)}/approvals/${encodeURIComponent(decisionId)}`, {
      method: 'DELETE'
    }),
  resumeDecisionApproval: (sessionId, decisionId) =>
    request(`/api/sessions/${encodeURIComponent(sessionId)}/approvals/${encodeURIComponent(decisionId)}/resume`, {
      method: 'POST', body: JSON.stringify({})
    }),
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
  // The files and the revision they were read at (the ETag, without its
  // quotes), for binding a later save with If-Match.
  getConfigFilesWithRevision: async () => {
    const { body, etag } = await request('/api/config/files', {}, { withEtag: true });
    return { files: body, revision: etag ? etag.replace(/^"|"$/g, '') : null };
  },
  // `confirmHighRisk` confirms a change the server classifies high risk
  // (a credential, `secrets.encrypt = false`, a provider endpoint); without
  // it such a save answers 428 with the reasons and the revision the refused
  // plan was computed against. `revision` is sent as `If-Match`, so the
  // save lands only on exactly that revision (409 otherwise).
  saveConfigFile: (filename, content, { confirmHighRisk = false, revision = null } = {}) =>
    request(`/api/config/files/${encodeURIComponent(filename)}${confirmSuffix(confirmHighRisk)}`, {
      method: 'PUT',
      headers: revisionHeaders(revision),
      body: JSON.stringify({ content })
    }),
  // The provider/model management plane (`/api/providers`, `/api/models`,
  // `/api/defaults`, `/api/routes`, `/api/agents`, `/api/config/*`). Every
  // typed change is planned (`planChange`) and then applied
  // (`applyChange`) with the plan's revision; a credential is sent once, in
  // a request body, never in a URL.
  getProviders: () => request('/api/providers'),
  getProvider: (provider) => request(`/api/providers/${encodeURIComponent(provider)}`),
  // Write-only: the value goes in the body of one PUT and the answer is an
  // empty 204.
  setProviderCredential: (provider, value, name) =>
    request(`/api/providers/${encodeURIComponent(provider)}/credential`, {
      method: 'PUT',
      body: JSON.stringify({ value, name: name || undefined })
    }),
  removeProviderCredential: (provider, name) =>
    request(`/api/providers/${encodeURIComponent(provider)}/credential${managementQuery([['name', name]])}`, {
      method: 'DELETE'
    }),
  // Local unless `live` is set; a live test sends one model-list request.
  testProvider: (provider, { model, live = false } = {}) =>
    request(`/api/providers/${encodeURIComponent(provider)}/test`, {
      method: 'POST',
      body: JSON.stringify({ model: model || undefined, live })
    }),
  getModels: ({ provider, operation, executable } = {}) =>
    request(
      `/api/models${managementQuery([
        ['provider', provider],
        ['operation', operation],
        ['executable', executable]
      ])}`
    ),
  getDefaults: () => request('/api/defaults'),
  getRoutes: (operation) => request(`/api/routes${managementQuery([['operation', operation]])}`),
  getAgents: () => request('/api/agents'),
  // Dry run: the plan text, its risk and the revision an apply names.
  planChange: (change) =>
    request('/api/config/plans', {
      method: 'POST',
      body: JSON.stringify({ change })
    }),
  applyChange: (change, { confirmHighRisk = false, revision = null } = {}) =>
    request(`/api/config/apply${confirmSuffix(confirmHighRisk)}`, {
      method: 'POST',
      headers: revisionHeaders(revision),
      body: JSON.stringify({ change })
    }),
  getConfigHistory: () => request('/api/config/history'),
  // `flags` are the confirmations a 428 asked for (`confirm`,
  // `restore_credentials`, `disable_encryption`); each counts only with
  // `If-Match` naming the revision that 428 reported.
  rollbackConfig: (recordId, flags = {}, { revision = null } = {}) =>
    request('/api/config/rollback', {
      method: 'POST',
      headers: revisionHeaders(revision),
      body: JSON.stringify({ record_id: recordId, ...flags })
    }),
  // Write-only credential slot (`api_key`, `model_routes.<hint>.api_key`,
  // ...): the value goes in the body and is never returned.
  setConfigSecret: (slot, value, { confirmHighRisk = false, revision = null } = {}) =>
    request(`/api/config/secrets${confirmSuffix(confirmHighRisk)}`, {
      method: 'PUT',
      headers: revisionHeaders(revision),
      body: JSON.stringify({ slot, value })
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
