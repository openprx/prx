const BEARER = /Bearer\s+\S+/gi;
const AUTH_HEADER = /\b(authorization\s*:\s*)(?!bearer\b)(?:"[^"]*"|'[^']*'|\S+(?:\s+\S+)?)/gi;
const JSON_SECRET = /("(?:api_key|apikey|token|password|secret|access_token|authorization)"\s*:\s*")[^"]*(")/gi;
const ASSIGNED_SECRET = /\b((?:api_key|token|password|secret|access_token)=)[^\s&]+/gi;
const TOKEN = /\b(?:sk|zc|ghp|gho|github_pat|xox[baprs])-[A-Za-z0-9_-]+/g;
const JWT = /\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b/g;

export function redactLogLine(line) {
  if (typeof line !== 'string') {
    return '';
  }
  return line
    .replace(BEARER, 'Bearer [REDACTED]')
    .replace(AUTH_HEADER, '$1[REDACTED]')
    .replace(JSON_SECRET, '$1[REDACTED]$2')
    .replace(ASSIGNED_SECRET, '$1[REDACTED]')
    .replace(TOKEN, '[REDACTED]')
    .replace(JWT, '[REDACTED]');
}

export function redactLogLines(lines) {
  return (Array.isArray(lines) ? lines : []).map((line) => redactLogLine(line));
}

export function exportRedactedLogs(lines) {
  return redactLogLines(lines).join('\n');
}
