import { expect, test } from 'vitest';
import { exportRedactedLogs, redactLogLine } from './log-redaction.js';

test('redacts bearer tokens, known token prefixes, and secret query values', () => {
  const line = redactLogLine(
    'Authorization: Bearer zc_live_secret sk-abc123 ghp-abcdef xoxb-123 used ?api_key=topsecret&token=other'
  );
  expect(line).not.toContain('zc_live_secret');
  expect(line).not.toContain('sk-abc123');
  expect(line).not.toContain('ghp-abcdef');
  expect(line).not.toContain('xoxb-123');
  expect(line).not.toContain('topsecret');
  expect(line).not.toContain('=other');
  expect(line).toContain('Bearer [REDACTED]');
  expect(line).toContain('api_key=[REDACTED]');
});

test('redacts json secret keys, authorization headers, and jwts', () => {
  const jwt = 'eyJhbGciOiJub25lIn0.eyJzdWIiOiJwcngifQ.c2lnbmF0dXJl';
  const line = redactLogLine(
    `{"api_key": "json-secret-value"} Authorization: Token header-secret api_key=assigned-secret ${jwt}`
  );
  expect(line).not.toContain('json-secret-value');
  expect(line).not.toContain('header-secret');
  expect(line).not.toContain('assigned-secret');
  expect(line).not.toContain(jwt);
  expect(line).toContain('"api_key": "[REDACTED]"');
  expect(line).toContain('Authorization: [REDACTED]');
  expect(line).toContain('api_key=[REDACTED]');
});

test('exports only the lines it is given', () => {
  expect(exportRedactedLogs(['plain', 'Bearer abc.def'])).toBe('plain\nBearer [REDACTED]');
});
