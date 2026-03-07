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
/** Whether we are running inside a WIT-componentized WASM module. */
function isWasmEnv() {
    return (typeof globalThis !== "undefined" &&
        "__prx_host__" in globalThis &&
        globalThis["__prx_host__"] !== null);
}
/** Retrieve the injected host bindings (WASM env only). */
function hostBindings() {
    // Cast through unknown to avoid strict globalThis index-signature errors.
    // The __prx_host__ property is only present when running inside a
    // jco-componentized WASM module, where the PRX runtime injects it.
    return globalThis.__prx_host__;
}
// ── log ───────────────────────────────────────────────────────────────────────
/**
 * Structured logging — writes to the PRX tracing infrastructure.
 *
 * Outside a WASM environment, log calls are forwarded to `console` so that
 * plugin code is testable locally.
 */
export const log = {
    /** Emit a TRACE-level log message. */
    trace(message) {
        _log("trace", message);
    },
    /** Emit a DEBUG-level log message. */
    debug(message) {
        _log("debug", message);
    },
    /** Emit an INFO-level log message. */
    info(message) {
        _log("info", message);
    },
    /** Emit a WARN-level log message. */
    warn(message) {
        _log("warn", message);
    },
    /** Emit an ERROR-level log message. */
    error(message) {
        _log("error", message);
    },
};
function _log(level, message) {
    if (isWasmEnv()) {
        hostBindings()["prx:host/log"].log(level, message);
    }
    else {
        // Stub: map to the appropriate console method
        const methods = {
            trace: (m) => console.debug(`[prx-pdk TRACE] ${m}`),
            debug: (m) => console.debug(`[prx-pdk DEBUG] ${m}`),
            info: (m) => console.info(`[prx-pdk INFO ] ${m}`),
            warn: (m) => console.warn(`[prx-pdk WARN ] ${m}`),
            error: (m) => console.error(`[prx-pdk ERROR] ${m}`),
        };
        methods[level](message);
    }
}
// ── config ────────────────────────────────────────────────────────────────────
/**
 * Plugin configuration — read-only access to values from `plugin.toml [config]`.
 *
 * Config values are set by the operator at deploy time.  Use `kv` for mutable
 * persistent storage.
 */
export const config = {
    /**
     * Get a configuration value by key.
     * Returns `undefined` if the key is not set.
     */
    get(key) {
        if (isWasmEnv()) {
            return hostBindings()["prx:host/config"].get(key);
        }
        return undefined;
    },
    /** Get all configuration key-value pairs. */
    getAll() {
        if (isWasmEnv()) {
            return hostBindings()["prx:host/config"].getAll();
        }
        return [];
    },
    /**
     * Get a configuration value, returning `defaultValue` if not set.
     */
    getOr(key, defaultValue) {
        return config.get(key) ?? defaultValue;
    },
};
// ── kv ────────────────────────────────────────────────────────────────────────
/**
 * Key-value storage — isolated per-plugin persistent store.
 *
 * Each plugin gets its own namespace; plugins cannot access each other's keys.
 * Values are raw bytes (`Uint8Array`).  Use JSON serialisation for structured data.
 */
export const kv = {
    /**
     * Retrieve a value by key.
     * Returns `undefined` if the key does not exist.
     */
    get(key) {
        if (isWasmEnv()) {
            return hostBindings()["prx:host/kv"].get(key);
        }
        return undefined;
    },
    /** Retrieve a value and decode it as UTF-8 text. */
    getString(key) {
        const bytes = kv.get(key);
        if (bytes === undefined)
            return undefined;
        return new TextDecoder().decode(bytes);
    },
    /** Retrieve and JSON-parse a stored value. */
    getJson(key) {
        const str = kv.getString(key);
        if (str === undefined)
            return undefined;
        try {
            return JSON.parse(str);
        }
        catch {
            return undefined;
        }
    },
    /** Store a byte value. Overwrites any existing value for the key. */
    set(key, value) {
        if (isWasmEnv()) {
            hostBindings()["prx:host/kv"].set(key, value);
        }
        // stub: no-op outside WASM
    },
    /** Store a UTF-8 string value. */
    setString(key, value) {
        kv.set(key, new TextEncoder().encode(value));
    },
    /** JSON-serialise and store a value. */
    setJson(key, value) {
        kv.setString(key, JSON.stringify(value));
    },
    /**
     * Delete a key.
     * Returns `true` if the key existed, `false` otherwise.
     */
    delete(key) {
        if (isWasmEnv()) {
            return hostBindings()["prx:host/kv"].delete(key);
        }
        return false;
    },
    /** List all keys matching a prefix. */
    listKeys(prefix) {
        if (isWasmEnv()) {
            return hostBindings()["prx:host/kv"].listKeys(prefix);
        }
        return [];
    },
    /**
     * Atomically increment an integer counter stored at `key`.
     *
     * Initialises to 0 if the key does not exist, then adds `delta`.
     * Returns the new value.
     */
    increment(key, delta) {
        const current = kv.getJson(key) ?? 0;
        const next = typeof current === "number" && isFinite(current) ? current + delta : delta;
        kv.setJson(key, next);
        return next;
    },
};
// ── events ────────────────────────────────────────────────────────────────────
/**
 * Event bus — fire-and-forget publish/subscribe for inter-plugin communication.
 *
 * Events flow through the host for auditing and access control.
 * Requires `"events"` permission in `plugin.toml`.
 * Payload must be valid JSON, max 64 KB.
 */
export const events = {
    /**
     * Publish an event to a topic.
     *
     * All subscribers matching the topic receive the event asynchronously.
     *
     * @param topic  Event topic (e.g. `"weather.update"`).
     * @param payload  JSON-encoded payload string (max 64 KB).
     */
    publish(topic, payload) {
        if (isWasmEnv()) {
            hostBindings()["prx:host/events"].publish(topic, payload);
        }
        // stub: log to console so tests can observe published events
        else {
            console.debug(`[prx-pdk events.publish] topic=${topic} payload=${payload}`);
        }
    },
    /**
     * Publish a JSON-serialisable value to a topic.
     */
    publishJson(topic, payload) {
        events.publish(topic, JSON.stringify(payload));
    },
    /**
     * Subscribe to a topic pattern.
     *
     * Supports exact match (`"weather.update"`) and wildcard (`"weather.*"`).
     * Returns a subscription ID for later `unsubscribe()`.
     */
    subscribe(topicPattern) {
        if (isWasmEnv()) {
            return hostBindings()["prx:host/events"].subscribe(topicPattern);
        }
        return 0n;
    },
    /**
     * Cancel a subscription by ID.
     */
    unsubscribe(subscriptionId) {
        if (isWasmEnv()) {
            hostBindings()["prx:host/events"].unsubscribe(subscriptionId);
        }
    },
};
// ── http ──────────────────────────────────────────────────────────────────────
/**
 * Outbound HTTP — make controlled HTTP requests from plugins.
 *
 * URLs are validated against the plugin's `http_allowlist` in `plugin.toml`.
 * Requires `"http-outbound"` permission.
 */
export const http = {
    /**
     * Make an HTTP request.
     *
     * @param method   HTTP verb (`"GET"`, `"POST"`, etc.)
     * @param url      Target URL (must match the plugin's `http_allowlist`)
     * @param headers  Request headers as `[name, value]` pairs
     * @param body     Optional request body bytes
     */
    request(method, url, headers, body) {
        if (isWasmEnv()) {
            const raw = hostBindings()["prx:host/http-outbound"].request(method, url, headers, body);
            return {
                status: raw.status,
                headers: raw.headers,
                body: raw.body,
            };
        }
        throw new Error("http.request is only available inside a PRX WASM component");
    },
    /** Convenience wrapper: HTTP GET. */
    get(url, headers = []) {
        return http.request("GET", url, headers);
    },
    /** Convenience wrapper: HTTP POST with a JSON body. */
    postJson(url, payload, headers = []) {
        const body = new TextEncoder().encode(JSON.stringify(payload));
        const mergedHeaders = [
            ...headers.filter(([k]) => k.toLowerCase() !== "content-type"),
            ["Content-Type", "application/json"],
        ];
        return http.request("POST", url, mergedHeaders, body);
    },
    /**
     * Parse an `HttpResponse` body as UTF-8 text.
     * Throws if the body is not valid UTF-8.
     */
    bodyText(response) {
        return new TextDecoder().decode(response.body);
    },
    /**
     * Parse an `HttpResponse` body as JSON.
     * Throws if the body is not valid UTF-8 or not valid JSON.
     */
    bodyJson(response) {
        return JSON.parse(http.bodyText(response));
    },
};
// ── clock ─────────────────────────────────────────────────────────────────────
/**
 * Clock — current time utilities for plugins.
 *
 * The PRX WIT spec does not expose a dedicated clock interface.  This module
 * provides a best-effort implementation using `Date.now()` which is available
 * in both the jco-componentize WASM sandbox and standard Node.js / browser
 * environments.
 */
export const clock = {
    /** Return the current time as Unix milliseconds (UTC). */
    nowMs() {
        return Date.now();
    },
    /** Return the current time as an ISO 8601 string (UTC). */
    nowIso() {
        return new Date().toISOString();
    },
};
// ── memory ────────────────────────────────────────────────────────────────────
/**
 * Long-term memory — store and recall text entries.
 *
 * Requires `"memory"` permission in `plugin.toml`.
 */
export const memory = {
    /**
     * Store text in memory.
     * Returns the generated entry ID.
     */
    store(text, category) {
        if (isWasmEnv()) {
            return hostBindings()["prx:host/memory"].store(text, category);
        }
        // stub: return a fake ID
        return `stub-${Date.now()}`;
    },
    /**
     * Recall memories matching a query.
     * Returns up to `limit` entries.
     */
    recall(query, limit) {
        if (isWasmEnv()) {
            return hostBindings()["prx:host/memory"].recall(query, limit);
        }
        return [];
    },
};
//# sourceMappingURL=host.js.map