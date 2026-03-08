import en from './en.json';
import ka from './ka.json';
import ru from './ru.json';
import zh from './zh.json';

export const LANG_STORAGE_KEY = 'prx-console-lang';
export const SUPPORTED_LANGUAGES = ['en', 'zh', 'ka', 'ru'];

const FALLBACK_LANG = 'en';
const dictionaries = { en, zh, ka, ru };

function normalizeLanguage(value) {
  if (typeof value !== 'string' || value.trim().length === 0) {
    return FALLBACK_LANG;
  }

  const normalized = value.trim().toLowerCase();
  if (normalized.startsWith('zh')) {
    return 'zh';
  }
  if (normalized.startsWith('ka')) {
    return 'ka';
  }
  if (normalized.startsWith('ru')) {
    return 'ru';
  }

  if (SUPPORTED_LANGUAGES.includes(normalized)) {
    return normalized;
  }

  return 'en';
}

function detectLanguage() {
  if (typeof window !== 'undefined') {
    const stored = window.localStorage.getItem(LANG_STORAGE_KEY);
    if (stored) {
      return normalizeLanguage(stored);
    }
  }

  if (typeof navigator !== 'undefined') {
    const browserLanguage = navigator.language || navigator.languages?.[0] || FALLBACK_LANG;
    return normalizeLanguage(browserLanguage);
  }

  return FALLBACK_LANG;
}

function resolvePathValue(dictionary, key) {
  return key.split('.').reduce((current, segment) => {
    if (!current || typeof current !== 'object') {
      return undefined;
    }

    return current[segment];
  }, dictionary);
}

function applyDocumentLanguage(lang) {
  if (typeof document !== 'undefined') {
    document.documentElement.lang =
      lang === 'zh' ? 'zh-CN' : lang === 'ka' ? 'ka-GE' : lang === 'ru' ? 'ru-RU' : 'en';
  }
}

function persistLanguage(lang) {
  if (typeof window !== 'undefined') {
    window.localStorage.setItem(LANG_STORAGE_KEY, lang);
  }
}

export const i18n = $state({
  lang: detectLanguage()
});

applyDocumentLanguage(i18n.lang);

export function setLanguage(nextLanguage) {
  const normalized = normalizeLanguage(nextLanguage);
  if (i18n.lang === normalized) {
    return;
  }

  i18n.lang = normalized;
  persistLanguage(normalized);
  applyDocumentLanguage(normalized);
}

export function toggleLanguage() {
  const currentIndex = SUPPORTED_LANGUAGES.indexOf(i18n.lang);
  const nextIndex = currentIndex >= 0 ? (currentIndex + 1) % SUPPORTED_LANGUAGES.length : 0;
  setLanguage(SUPPORTED_LANGUAGES[nextIndex]);
}

export function syncLanguageFromStorage() {
  if (typeof window === 'undefined') {
    return;
  }

  const stored = window.localStorage.getItem(LANG_STORAGE_KEY);
  if (!stored) {
    return;
  }

  setLanguage(stored);
}

export function t(key, values = {}) {
  const active = dictionaries[i18n.lang] ?? dictionaries[FALLBACK_LANG];
  let output = resolvePathValue(active, key);

  if (typeof output !== 'string') {
    output = resolvePathValue(dictionaries[FALLBACK_LANG], key);
  }

  if (typeof output !== 'string') {
    return key;
  }

  for (const [name, value] of Object.entries(values)) {
    output = output.replaceAll(`{${name}}`, String(value));
  }

  return output;
}
