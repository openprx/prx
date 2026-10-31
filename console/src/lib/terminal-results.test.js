import { expect, test } from 'vitest';
import { resultsWereTrimmed } from './terminal-results';

test('a fresh cursor and the cursor just before oldest_seq are not missed', () => {
  expect(resultsWereTrimmed(0, 1)).toBe(false);
  expect(resultsWereTrimmed(1, 1)).toBe(false);
  expect(resultsWereTrimmed(5, 5)).toBe(false);
  expect(resultsWereTrimmed(1, 2)).toBe(false);
  expect(resultsWereTrimmed(4, 5)).toBe(false);
});

test('a non-zero cursor at least two behind oldest_seq was trimmed', () => {
  expect(resultsWereTrimmed(1, 3)).toBe(true);
  expect(resultsWereTrimmed(4, 6)).toBe(true);
});
