/**
 * PRX PDK — Host function wrappers.
 *
 * This module provides ergonomic TypeScript wrappers around the WIT-generated
 * host function bindings exposed by the PRX runtime.
 *
 * ## WASM vs non-WASM environments
 *
 * When compiled to a WASM component via `jco componentize`, the PRX runtime
 * injects the real host implementations as WIT imports.  In that environment
 * the wrapper functions delegate to the generated bindings.
 *
 * Outside a WASM component (e.g. during unit tests or local development),
 * the WIT bindings are unavailable.  The wrappers fall back to harmless stubs
 * (console.log for logging, no-ops for storage/events) so that plugin code can
 * be tested without a running PRX host.
 *
 * Detection is performed via `globalThis.__prx_host__`, which the
 * jco-componentize runtime sets to the bound host exports object.
 */
import type { HttpResponse, MemoryEntry } from "./types.js";
/**
 * Structured logging — writes to the PRX tracing infrastructure.
 *
 * Outside a WASM environment, log calls are forwarded to `console` so that
 * plugin code is testable locally.
 */
export declare const log: {
    /** Emit a TRACE-level log message. */
    readonly trace: (message: string) => void;
    /** Emit a DEBUG-level log message. */
    readonly debug: (message: string) => void;
    /** Emit an INFO-level log message. */
    readonly info: (message: string) => void;
    /** Emit a WARN-level log message. */
    readonly warn: (message: string) => void;
    /** Emit an ERROR-level log message. */
    readonly error: (message: string) => void;
};
/**
 * Plugin configuration — read-only access to values from `plugin.toml [config]`.
 *
 * Config values are set by the operator at deploy time.  Use `kv` for mutable
 * persistent storage.
 */
export declare const config: {
    /**
     * Get a configuration value by key.
     * Returns `undefined` if the key is not set.
     */
    readonly get: (key: string) => string | undefined;
    /** Get all configuration key-value pairs. */
    readonly getAll: () => [string, string][];
    /**
     * Get a configuration value, returning `defaultValue` if not set.
     */
    readonly getOr: (key: string, defaultValue: string) => string;
};
/**
 * Key-value storage — isolated per-plugin persistent store.
 *
 * Each plugin gets its own namespace; plugins cannot access each other's keys.
 * Values are raw bytes (`Uint8Array`).  Use JSON serialisation for structured data.
 */
export declare const kv: {
    /**
     * Retrieve a value by key.
     * Returns `undefined` if the key does not exist.
     */
    readonly get: (key: string) => Uint8Array | undefined;
    /** Retrieve a value and decode it as UTF-8 text. */
    readonly getString: (key: string) => string | undefined;
    /** Retrieve and JSON-parse a stored value. */
    readonly getJson: <T>(key: string) => T | undefined;
    /** Store a byte value. Overwrites any existing value for the key. */
    readonly set: (key: string, value: Uint8Array) => void;
    /** Store a UTF-8 string value. */
    readonly setString: (key: string, value: string) => void;
    /** JSON-serialise and store a value. */
    readonly setJson: (key: string, value: unknown) => void;
    /**
     * Delete a key.
     * Returns `true` if the key existed, `false` otherwise.
     */
    readonly delete: (key: string) => boolean;
    /** List all keys matching a prefix. */
    readonly listKeys: (prefix: string) => string[];
    /**
     * Atomically increment an integer counter stored at `key`.
     *
     * Initialises to 0 if the key does not exist, then adds `delta`.
     * Returns the new value.
     */
    readonly increment: (key: string, delta: number) => number;
};
/**
 * Event bus — fire-and-forget publish/subscribe for inter-plugin communication.
 *
 * Events flow through the host for auditing and access control.
 * Requires `"events"` permission in `plugin.toml`.
 * Payload must be valid JSON, max 64 KB.
 */
export declare const events: {
    /**
     * Publish an event to a topic.
     *
     * All subscribers matching the topic receive the event asynchronously.
     *
     * @param topic  Event topic (e.g. `"weather.update"`).
     * @param payload  JSON-encoded payload string (max 64 KB).
     */
    readonly publish: (topic: string, payload: string) => void;
    /**
     * Publish a JSON-serialisable value to a topic.
     */
    readonly publishJson: (topic: string, payload: unknown) => void;
    /**
     * Subscribe to a topic pattern.
     *
     * Supports exact match (`"weather.update"`) and wildcard (`"weather.*"`).
     * Returns a subscription ID for later `unsubscribe()`.
     */
    readonly subscribe: (topicPattern: string) => bigint;
    /**
     * Cancel a subscription by ID.
     */
    readonly unsubscribe: (subscriptionId: bigint) => void;
};
/**
 * Outbound HTTP — make controlled HTTP requests from plugins.
 *
 * URLs are validated against the plugin's `http_allowlist` in `plugin.toml`.
 * Requires `"http-outbound"` permission.
 */
export declare const http: {
    /**
     * Make an HTTP request.
     *
     * @param method   HTTP verb (`"GET"`, `"POST"`, etc.)
     * @param url      Target URL (must match the plugin's `http_allowlist`)
     * @param headers  Request headers as `[name, value]` pairs
     * @param body     Optional request body bytes
     */
    readonly request: (method: string, url: string, headers: [string, string][], body?: Uint8Array) => HttpResponse;
    /** Convenience wrapper: HTTP GET. */
    readonly get: (url: string, headers?: [string, string][]) => HttpResponse;
    /** Convenience wrapper: HTTP POST with a JSON body. */
    readonly postJson: (url: string, payload: unknown, headers?: [string, string][]) => HttpResponse;
    /**
     * Parse an `HttpResponse` body as UTF-8 text.
     * Throws if the body is not valid UTF-8.
     */
    readonly bodyText: (response: HttpResponse) => string;
    /**
     * Parse an `HttpResponse` body as JSON.
     * Throws if the body is not valid UTF-8 or not valid JSON.
     */
    readonly bodyJson: <T>(response: HttpResponse) => T;
};
/**
 * Clock — current time utilities for plugins.
 *
 * The PRX WIT spec does not expose a dedicated clock interface.  This module
 * provides a best-effort implementation using `Date.now()` which is available
 * in both the jco-componentize WASM sandbox and standard Node.js / browser
 * environments.
 */
export declare const clock: {
    /** Return the current time as Unix milliseconds (UTC). */
    readonly nowMs: () => number;
    /** Return the current time as an ISO 8601 string (UTC). */
    readonly nowIso: () => string;
};
/**
 * Long-term memory — store and recall text entries.
 *
 * Requires `"memory"` permission in `plugin.toml`.
 */
export declare const memory: {
    /**
     * Store text in memory.
     * Returns the generated entry ID.
     */
    readonly store: (text: string, category: string) => string;
    /**
     * Recall memories matching a query.
     * Returns up to `limit` entries.
     */
    readonly recall: (query: string, limit: number) => MemoryEntry[];
};
//# sourceMappingURL=host.d.ts.map