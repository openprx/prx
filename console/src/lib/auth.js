import { TOKEN_STORAGE_KEY } from './constants';

const TOKEN_COOKIE_NAME = 'prx_console_token';

function buildCookieAttributes() {
  const attributes = ['Path=/', 'SameSite=Strict'];
  if (typeof window !== 'undefined' && window.location.protocol === 'https:') {
    attributes.push('Secure');
  }
  return attributes.join('; ');
}

function writeTokenCookie(token) {
  if (typeof document === 'undefined') {
    return;
  }

  const normalized = token.trim();
  if (!normalized) {
    document.cookie = `${TOKEN_COOKIE_NAME}=; Path=/; Max-Age=0; SameSite=Strict`;
    return;
  }

  document.cookie = `${TOKEN_COOKIE_NAME}=${encodeURIComponent(normalized)}; ${buildCookieAttributes()}`;
}

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

  const normalized = token.trim();
  window.localStorage.setItem(TOKEN_STORAGE_KEY, normalized);
  writeTokenCookie(normalized);
}

export function clearToken() {
  if (typeof window === 'undefined') {
    return;
  }

  window.localStorage.removeItem(TOKEN_STORAGE_KEY);
  writeTokenCookie('');
}

export function syncTokenCookie() {
  writeTokenCookie(getToken());
}
