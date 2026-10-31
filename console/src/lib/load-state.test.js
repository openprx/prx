import { expect, test } from 'vitest';
import { channelSendOutcome, loadFailureVariant, pluginActionKind } from './load-state';

test('maps a forbidden load to denied and every other load failure to error', () => {
  expect(loadFailureVariant({ status: 403 })).toBe('denied');
  expect(loadFailureVariant({ status: 500 })).toBe('error');
  expect(loadFailureVariant(new TypeError('offline'))).toBe('error');
});

test('treats a channel send without an HTTP status as uncertain', () => {
  expect(channelSendOutcome({ status: 403 })).toBe('denied');
  expect(channelSendOutcome({ status: 400 })).toBe('error');
  expect(channelSendOutcome(new DOMException('timed out', 'TimeoutError'))).toBe('uncertain');
  expect(channelSendOutcome(new TypeError('network'))).toBe('uncertain');
});

test('maps plugin action statuses without treating them as success', () => {
  expect(pluginActionKind({ status: 403 })).toBe('denied');
  expect(pluginActionKind({ status: 409 })).toBe('conflict');
  expect(pluginActionKind({ status: 428 })).toBe('confirm');
  expect(pluginActionKind({ status: 404 })).toBe('absent');
  expect(pluginActionKind({ status: 503 })).toBe('unavailable');
  expect(pluginActionKind({ status: 500 })).toBe('error');
});
