import { TOKEN_STORAGE_KEY } from './constants';

export function getToken() {
  if (typeof window === 'undefined') {
    return '';
  }

  return window.localStorage.getItem(TOKEN_STORAGE_KEY)?.trim() ?? '';
}

export function setToken(token) {
  if (typeof window === 'undefined') {
    return;
  }

  window.localStorage.setItem(TOKEN_STORAGE_KEY, token.trim());
}

export function clearToken() {
  if (typeof window === 'undefined') {
    return;
  }

  window.localStorage.removeItem(TOKEN_STORAGE_KEY);
}
