# OpenPRX 0.8.17

This patch records the post-audit configuration remediation and refreshes the
release artifact after the 0.8.16 regression audit.

- Adds explicit operator acknowledgements for intentional unrestricted
  autonomy and single-provider deployments, keeping doctor output actionable.
- Removes HTTP request domain allowlists and permission gates: `http_request`
  is a directly usable native network primitive. Timeout and response-size
  limits remain operational transport settings.
- Preserves always-registered MCP, HTTP request, web search, and web fetch
  surfaces while keeping Causal Tree as the only intentionally gated feature.
- Carries forward the shell direct-execution, audit-log, gateway-health,
  configuration-template, MCP-schema, and telemetry fixes from 0.8.16.

Regression evidence for this release includes the full openprx library suite,
focused doctor and configuration-template tests, release compilation, deployed
runtime checks, and tmux `demo` K3 verification with zero functional failures.
