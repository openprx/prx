// How a failed page load is shown. A 403 is a refusal; every other failure,
// including a network error, is a load error.
export function loadFailureVariant(error) {
  return error?.status === 403 ? 'denied' : 'error';
}

// Channel send: an HTTP status is a finished answer. Anything without one
// (abort, timeout, network) did not produce a delivery result.
export function channelSendOutcome(error) {
  if (error?.status === 403) {
    return 'denied';
  }
  if (typeof error?.status === 'number') {
    return 'error';
  }
  return 'uncertain';
}

// Plugin mutations. 428 means the high-risk confirmation was missing.
export function pluginActionKind(error) {
  if (error?.status === 403) return 'denied';
  if (error?.status === 409) return 'conflict';
  if (error?.status === 428) return 'confirm';
  if (error?.status === 404) return 'absent';
  if (error?.status === 503) return 'unavailable';
  return 'error';
}
