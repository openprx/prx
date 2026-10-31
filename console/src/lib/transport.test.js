import { expect, test } from 'vitest';
import { isAmbiguousTransport, pollDelayMs, sameOriginApiPath } from './transport';

test('caps the poll delay at eight seconds', () => {
  expect(pollDelayMs(0)).toBe(1000);
  expect(pollDelayMs(1)).toBe(1500);
  expect(pollDelayMs(6)).toBe(8000);
  expect(pollDelayMs(20)).toBe(8000);
});

test('accepts only same-origin API paths', () => {
  expect(sameOriginApiPath('/api/jobs/abc')).toBe('/api/jobs/abc');
  expect(sameOriginApiPath('https://example.com/api/jobs/abc')).toBeNull();
  expect(sameOriginApiPath('//example.com/api/jobs/abc')).toBeNull();
  expect(sameOriginApiPath('/sessions')).toBeNull();
});

test('treats a typed HTTP status as a known answer', () => {
  expect(isAmbiguousTransport({ status: 403 })).toBe(false);
  expect(isAmbiguousTransport(new TypeError('failed'))).toBe(true);
});
