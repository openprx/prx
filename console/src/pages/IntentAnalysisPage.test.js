import { render, screen } from '@testing-library/svelte';
import { beforeEach, expect, test, vi } from 'vitest';
import IntentAnalysisPage from './IntentAnalysisPage.svelte';
import { api } from '../lib/api';

vi.mock('../lib/api', () => ({
  ApiError: class ApiError extends Error {
    constructor(status, message) {
      super(message);
      this.status = status;
    }
  },
  api: {
    getDecisionEngineStatus: vi.fn(),
    getDecisionEngineBackends: vi.fn(),
    getDecisionEngineAnalyses: vi.fn(),
    getConfigFilesWithRevision: vi.fn(),
    storeDecisionEngineCredential: vi.fn()
  }
}));

beforeEach(() => {
  api.getDecisionEngineStatus.mockReset();
  api.getDecisionEngineBackends.mockReset();
  api.getDecisionEngineAnalyses.mockReset();
});

test('renders disabled state and redacted briefs', async () => {
  api.getDecisionEngineStatus.mockResolvedValue({
    enabled: false,
    mode: 'off',
    backend_kind: 'builtin',
    telemetry_cap: 0,
    queue_depth: 0,
    dropped: 0,
    store_errors: 0
  });
  api.getDecisionEngineBackends.mockResolvedValue([
    { kind: 'builtin', locality: 'local', model: 'builtin-v1', pinned_snapshot: null, credential_configured: false, circuit: 'closed' }
  ]);
  api.getDecisionEngineAnalyses.mockResolvedValue([
    {
      decision_id: '0192f0c4-0000-7000-8000-000000000001',
      created_at_ms: 0,
      brief_redacted: 'deploy to [REDACTED]',
      brief_sha256: 'a'.repeat(64),
      global: {},
      candidates: [{ tool: 'shell' }]
    }
  ]);
  render(IntentAnalysisPage);
  expect(await screen.findByText('The decision engine is off. No new analyses are recorded.')).toBeInTheDocument();
  expect(screen.getByText('Brief: deploy to [REDACTED]')).toBeInTheDocument();
  expect(screen.getByText('builtin / local / builtin-v1 · Credential configured: false')).toBeInTheDocument();
  expect(screen.getByText('Candidates: 1')).toBeInTheDocument();
  expect(api.getDecisionEngineAnalyses).toHaveBeenCalledWith({ limit: 20 });
});
