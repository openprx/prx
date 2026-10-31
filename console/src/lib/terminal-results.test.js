import { expect, test } from 'vitest';
import { resultsWereTrimmed } from './terminal-results';

test('a fresh cursor and an adjacent retained cursor are not missed', () => {
  expect(resultsWereTrimmed(0, 1)).toBe(false);
  expect(resultsWereTrimmed(1, 1)).toBe(false);
  expect(resultsWereTrimmed(5, 5)).toBe(false);
});

test('a non-zero cursor older than oldest_seq was trimmed', () => {
  expect(resultsWereTrimmed(1, 2)).toBe(true);
  expect(resultsWereTrimmed(4, 5)).toBe(true);
});
