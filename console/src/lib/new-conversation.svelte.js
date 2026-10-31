import { ApiError, api } from './api';
import { currentPath, navigate } from './router';
import { isAmbiguousTransport } from './transport';

export const sessionCreate = $state({
  pending: false,
  error: '',
  uncertain: false
});

let inflight = null;
const createdListeners = new Set();
const refreshListeners = new Set();

export function subscribeSessionCreated(listener) {
  createdListeners.add(listener);
  return () => createdListeners.delete(listener);
}

export function subscribeListRefresh(listener) {
  refreshListeners.add(listener);
  return () => refreshListeners.delete(listener);
}

export function resetSessionCreateState() {
  if (inflight) {
    return;
  }
  sessionCreate.pending = false;
  sessionCreate.error = '';
  sessionCreate.uncertain = false;
}

async function notify(listeners, value) {
  for (const listener of listeners) {
    try {
      await listener(value);
    } catch {
      // A list refresh must not block opening the conversation.
    }
  }
}

export function createConversation() {
  if (inflight) {
    return inflight;
  }
  sessionCreate.pending = true;
  sessionCreate.error = '';
  sessionCreate.uncertain = false;
  inflight = (async () => {
    try {
      const response = await api.createSession();
      const status = response?.status;
      const sessionId = typeof response?.body?.session_id === 'string' ? response.body.session_id.trim() : '';
      if (Number.isInteger(status) && status >= 200 && status < 300 && sessionId.length > 0) {
        await notify(createdListeners, sessionId);
        navigate(`/chat/${encodeURIComponent(sessionId)}`);
        return;
      }
      sessionCreate.uncertain = true;
    } catch (failure) {
      if (failure instanceof ApiError) {
        sessionCreate.error = failure.message;
        sessionCreate.uncertain = false;
      } else if (isAmbiguousTransport(failure)) {
        sessionCreate.uncertain = true;
        sessionCreate.error = '';
      } else {
        sessionCreate.error = failure instanceof Error ? failure.message : '';
      }
    } finally {
      sessionCreate.pending = false;
      inflight = null;
    }
  })();
  return inflight;
}

export function refreshConversationList() {
  if (currentPath() !== '/sessions') {
    navigate('/sessions');
  }
  for (const listener of refreshListeners) {
    try {
      const result = listener();
      if (result && typeof result.catch === 'function') {
        result.catch(() => {});
      }
    } catch {
      // The banner stays. Refresh is explicit and is not a second create.
    }
  }
}
