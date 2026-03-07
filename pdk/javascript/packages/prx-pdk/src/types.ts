/**
 * PRX PDK — TypeScript type definitions.
 *
 * All interfaces mirror the WIT records defined in `wit/plugin/*.wit` and
 * `wit/host/*.wit` exactly.  Keep these in sync whenever the WIT definitions
 * change.
 */

// ── plugin/tool-exports (wit/plugin/tool.wit) ────────────────────────────────

/**
 * Tool specification returned from `getSpec()`.
 *
 * Mirrors WIT record `tool-spec` in `prx:plugin/tool-exports`.
 */
export interface ToolSpec {
  /** Tool name (snake_case). Shown to the LLM as the callable function name. */
  name: string;
  /** Human-readable description shown to the LLM. */
  description: string;
  /**
   * JSON Schema string describing the tool's input parameters.
   * Must be a valid JSON object schema.
   */
  parametersSchema: string;
}

/**
 * Result returned from `execute()` / `run()` calls.
 *
 * Mirrors WIT record `plugin-result` in `prx:plugin/tool-exports`.
 */
export interface PluginResult {
  /** Whether the operation succeeded. */
  success: boolean;
  /** Output text (may be empty on error). */
  output: string;
  /** Optional error message (populated when `success === false`). */
  error?: string;
}

// ── plugin/middleware-exports (wit/plugin/middleware.wit) ────────────────────

/**
 * Action returned by middleware plugins after processing a stage.
 *
 * This is the TypeScript representation of the middleware result pattern.
 * When `action === "continue"`, the `data` field carries the (possibly
 * modified) JSON string to pass downstream.  When `action === "block"`,
 * the pipeline is halted with the given `reason`.
 */
export type MiddlewareAction =
  | { action: "continue"; data: string }
  | { action: "block"; reason: string };

// ── host/http-outbound (wit/host/http.wit) ───────────────────────────────────

/**
 * HTTP response from an outbound request.
 *
 * Mirrors WIT record `http-response` in `prx:host/http-outbound`.
 */
export interface HttpResponse {
  /** HTTP status code (e.g. 200, 404). */
  status: number;
  /** Response headers as `[name, value]` pairs. */
  headers: [string, string][];
  /** Response body as raw bytes. */
  body: Uint8Array;
}

// ── host/memory (wit/host/memory.wit) ────────────────────────────────────────

/**
 * A memory entry returned by `memory.recall()`.
 *
 * Mirrors WIT record `memory-entry` in `prx:host/memory`.
 */
export interface MemoryEntry {
  /** Unique entry ID (opaque string). */
  id: string;
  /** Stored text content. */
  text: string;
  /** Category label (e.g. `"fact"`, `"preference"`, `"decision"`). */
  category: string;
  /** Importance score in the range [0.0, 1.0]. */
  importance: number;
}

// ── host/log (wit/host/log.wit) ───────────────────────────────────────────────

/**
 * Log severity level.
 *
 * Mirrors WIT enum `level` in `prx:host/log`.
 */
export type LogLevel = "trace" | "debug" | "info" | "warn" | "error";

// ── Cron context ──────────────────────────────────────────────────────────────

/**
 * Context passed to cron plugins when the schedule fires.
 *
 * This type is not part of the WIT interface directly (the `run` function takes
 * no arguments), but is provided as a convenience for plugins that need to
 * track scheduling information via KV storage.
 */
export interface CronContext {
  /** ISO 8601 timestamp of when this invocation was triggered. */
  firedAt: string;
  /** The cron expression from the plugin manifest (if available). */
  cronExpr?: string;
}

// ── Convenience result helpers ────────────────────────────────────────────────

/** Create a successful `PluginResult`. */
export function resultOk(output: string): PluginResult {
  return { success: true, output };
}

/** Create a failure `PluginResult`. */
export function resultErr(error: string): PluginResult {
  return { success: false, output: "", error };
}

/** Create a "continue" `MiddlewareAction` with the given data. */
export function middlewareContinue(data: string): MiddlewareAction {
  return { action: "continue", data };
}

/** Create a "block" `MiddlewareAction` with the given reason. */
export function middlewareBlock(reason: string): MiddlewareAction {
  return { action: "block", reason };
}
