export const POLL_MIN_MS = 1000;
export const POLL_MAX_MS = 8000;
export const STILL_RUNNING_MS = 15 * 60 * 1000;
export const PIN_GAP_PX = 80;

export function pollDelayMs(attempt) {
  const step = Number.isFinite(attempt) && attempt > 0 ? attempt : 0;
  const scaled = Math.round(POLL_MIN_MS * 1.5 ** step);
  if (!Number.isFinite(scaled)) {
    return POLL_MAX_MS;
  }
  return Math.min(POLL_MAX_MS, Math.max(POLL_MIN_MS, scaled));
}

// Only a same-origin gateway path may be followed. Anything with a scheme,
// a protocol-relative prefix, or a non-API path is ignored by the caller.
export function sameOriginApiPath(value) {
  if (typeof value !== 'string') {
    return null;
  }
  const path = value.trim();
  if (path.length === 0 || path.includes('://') || path.startsWith('//') || !path.startsWith('/api/')) {
    return null;
  }
  if (path.includes('\\') || path.includes(' ')) {
    return null;
  }
  return path;
}

export function isAmbiguousTransport(error) {
  return !(error && Number.isInteger(error.status));
}

export function isTerminalPhase(status) {
  return status === 'succeeded' || status === 'failed' || status === 'cancelled';
}
