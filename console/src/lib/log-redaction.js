const BEARER = /Bearer\s+\S+/gi;
const TOKEN = /\b(?:sk|zc|ghp|gho|github_pat|xox[baprs])-[A-Za-z0-9_-]+/g;
const QUERY_SECRET = /([?&](?:api_key|token|password|secret|access_token)=)[^&\s]+/gi;

export function redactLogLine(line) {
  if (typeof line !== 'string') {
    return '';
  }
  return line.replace(BEARER, 'Bearer [REDACTED]').replace(TOKEN, '[REDACTED]').replace(QUERY_SECRET, '$1[REDACTED]');
}

export function redactLogLines(lines) {
  return (Array.isArray(lines) ? lines : []).map((line) => redactLogLine(line));
}

export function exportRedactedLogs(lines) {
  return redactLogLines(lines).join('\n');
}
