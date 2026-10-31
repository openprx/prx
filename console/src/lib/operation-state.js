import { ApiError } from './api';

export function operationFailure(error) {
  const message = error instanceof Error && error.message.trim().length > 0 ? error.message : '';
  if (error instanceof ApiError && error.status === 403) {
    return { variant: 'denied', message };
  }
  if (error instanceof ApiError && error.status === 503) {
    return { variant: 'unavailable', message };
  }
  return { variant: 'error', message };
}
