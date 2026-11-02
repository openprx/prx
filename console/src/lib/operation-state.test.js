import { expect, test } from 'vitest';
import { ApiError } from './api.js';
import { operationFailure } from './operation-state.js';

test('maps forbidden and unavailable responses and leaves other failures as errors', () => {
  expect(operationFailure(new ApiError(403, 'read-only')).variant).toBe('denied');
  expect(operationFailure(new ApiError(503, 'down')).variant).toBe('unavailable');
  expect(operationFailure(new ApiError(409, 'conflict')).variant).toBe('error');
  expect(operationFailure(new ApiError(500, 'uncertain')).variant).toBe('error');
  expect(operationFailure(new Error('broken')).message).toBe('broken');
});
