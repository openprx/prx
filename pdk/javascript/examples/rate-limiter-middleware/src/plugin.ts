/**
 * rate-limiter-middleware — PRX Middleware Plugin Example (JavaScript/TypeScript PDK)
 *
 * Implements per-user rate limiting using a sliding-window token-bucket approach.
 * State is stored in the plugin's KV store (isolated per-plugin namespace).
 *
 * ## Configuration (plugin.toml [config])
 *
 * | Key               | Default | Description                              |
 * |-------------------|---------|------------------------------------------|
 * | max_requests      | 60      | Maximum requests per window              |
 * | window_seconds    | 60      | Sliding window size in seconds           |
 * | user_id_field     | user_id | JSON field name to extract the user ID   |
 * | block_message     | (built-in) | Message returned when rate-limited   |
 *
 * ## Build
 *
 * ```sh
 * npm install
 * npm run build:wasm
 * ```
 */

import { log, config, kv, clock, resultErr, middlewareContinue, middlewareBlock } from "@prx/pdk";
import type { PluginResult } from "@prx/pdk";

// ── Configuration ─────────────────────────────────────────────────────────────

interface RateLimiterConfig {
  maxRequests: number;
  windowSeconds: number;
  userIdField: string;
  blockMessage: string;
}

function loadConfig(): RateLimiterConfig {
  return {
    maxRequests: parseInt(config.getOr("max_requests", "60"), 10),
    windowSeconds: parseInt(config.getOr("window_seconds", "60"), 10),
    userIdField: config.getOr("user_id_field", "user_id"),
    blockMessage: config.getOr(
      "block_message",
      "Rate limit exceeded. Please wait before making more requests.",
    ),
  };
}

// ── Sliding window state stored in KV ─────────────────────────────────────────

interface WindowState {
  /** Timestamps (ms) of requests within the current window. */
  timestamps: number[];
}

function kvKeyForUser(userId: string): string {
  // Sanitise userId to avoid KV key injection
  const safe = userId.replace(/[^a-zA-Z0-9_\-.:@]/g, "_").slice(0, 128);
  return `ratelimit:${safe}`;
}

function isRateLimited(userId: string, cfg: RateLimiterConfig): boolean {
  const key = kvKeyForUser(userId);
  const now = clock.nowMs();
  const windowMs = cfg.windowSeconds * 1000;
  const cutoff = now - windowMs;

  // Load existing state
  const state = kv.getJson<WindowState>(key) ?? { timestamps: [] };

  // Evict timestamps outside the window
  const fresh = state.timestamps.filter((ts) => ts >= cutoff);

  if (fresh.length >= cfg.maxRequests) {
    // Do NOT add the timestamp — the request is blocked
    kv.setJson(key, { timestamps: fresh } satisfies WindowState);
    return true;
  }

  // Record this request
  fresh.push(now);
  kv.setJson(key, { timestamps: fresh } satisfies WindowState);
  return false;
}

// ── Middleware process ─────────────────────────────────────────────────────────

export function process(stage: string, dataJson: string): PluginResult {
  // Only apply rate limiting on inbound messages
  if (stage !== "inbound") {
    return { success: true, output: dataJson };
  }

  const cfg = loadConfig();

  // Parse the incoming message to find the user ID
  let data: Record<string, unknown>;
  try {
    data = JSON.parse(dataJson) as Record<string, unknown>;
  } catch (e) {
    // Cannot parse → pass through unchanged (do not block)
    log.warn(`rate-limiter: failed to parse data JSON: ${String(e)}`);
    return { success: true, output: dataJson };
  }

  const userId = extractUserId(data, cfg.userIdField);
  if (!userId) {
    // No user ID present → pass through (cannot rate-limit anonymous)
    log.debug("rate-limiter: no user_id found, passing through");
    return { success: true, output: dataJson };
  }

  if (isRateLimited(userId, cfg)) {
    log.warn(
      `rate-limiter: user "${userId}" exceeded ${cfg.maxRequests} req / ${cfg.windowSeconds}s`,
    );
    // Track total blocked requests
    kv.increment("blocked_total", 1);

    // Return a "block" action as JSON in the output — the PRX middleware
    // chain interprets this as a pipeline halt.
    const action = middlewareBlock(cfg.blockMessage);
    return { success: true, output: JSON.stringify(action) };
  }

  log.debug(`rate-limiter: user "${userId}" allowed`);
  kv.increment("allowed_total", 1);

  // Pass through the unchanged data
  const action = middlewareContinue(dataJson);
  return { success: true, output: JSON.stringify(action) };
}

// ── Helper: extract user ID from arbitrary JSON ───────────────────────────────

function extractUserId(
  data: Record<string, unknown>,
  field: string,
): string | undefined {
  // Support dot-notation for nested fields: e.g. "metadata.user_id"
  const parts = field.split(".");
  let cursor: unknown = data;
  for (const part of parts) {
    if (
      cursor === null ||
      typeof cursor !== "object" ||
      !(part in (cursor as Record<string, unknown>))
    ) {
      return undefined;
    }
    cursor = (cursor as Record<string, unknown>)[part];
  }
  if (typeof cursor === "string" && cursor.length > 0) return cursor;
  if (typeof cursor === "number") return String(cursor);
  return undefined;
}

// ── Diagnostics helper (callable via LLM for debugging) ───────────────────────

/**
 * Return current rate-limiter stats as a JSON string.
 * This is NOT a WIT export; it is a utility for local testing.
 */
export function getStats(): string {
  const allowed = kv.getJson<number>("allowed_total") ?? 0;
  const blocked = kv.getJson<number>("blocked_total") ?? 0;
  return JSON.stringify({ allowed, blocked });
}

// ── Unused export to satisfy the middleware WIT world ─────────────────────────
// The `middleware` world only requires `process(stage, data-json) → result<string, string>`.
// The actual WIT binding wiring is done in the wasm_exports block below (wasm32 only).

// ── Type-level notes ─────────────────────────────────────────────────────────
//
// PluginResult is used here instead of the raw WIT `result<string, string>` because
// @prx/pdk wraps the WIT middleware-exports in the same PluginResult pattern
// (success=true → the output string is the possibly-modified data or a JSON-encoded
// MiddlewareAction; success=false → error string).
//
// The PRX host runtime inspects the output for a JSON-encoded MiddlewareAction:
//   { "action": "block",    "reason": "..." }  → halt pipeline, return error
//   { "action": "continue", "data": "..."   }  → pass modified data downstream
// Any other output is treated as raw "continue" data.

// Silence unused import warnings in non-WASM builds
void (resultErr as unknown);
