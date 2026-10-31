import { expect, test } from 'vitest';
import en from './en.json';
import ka from './ka.json';
import ru from './ru.json';
import zh from './zh.json';
import { setLanguage, t } from './index.js';

function leaves(node, prefix = '') {
  const paths = [];
  for (const [key, value] of Object.entries(node)) {
    const next = prefix ? `${prefix}.${key}` : key;
    if (value && typeof value === 'object') {
      paths.push(...leaves(value, next));
    } else {
      paths.push(next);
    }
  }
  return paths;
}

test('returns the english app title and echoes a missing key', () => {
  setLanguage('en');
  expect(t('app.title')).toBe('PRX Console');
  expect(t('missing.key')).toBe('missing.key');
});

test('keeps the same leaf paths in every locale', () => {
  const expected = leaves(en).sort();
  expect(leaves(zh).sort()).toEqual(expected);
  expect(leaves(ka).sort()).toEqual(expected);
  expect(leaves(ru).sort()).toEqual(expected);
  expect(expected.length).toBeGreaterThanOrEqual(605);
});
