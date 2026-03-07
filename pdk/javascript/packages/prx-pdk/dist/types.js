/**
 * PRX PDK — TypeScript type definitions.
 *
 * All interfaces mirror the WIT records defined in `wit/plugin/*.wit` and
 * `wit/host/*.wit` exactly.  Keep these in sync whenever the WIT definitions
 * change.
 */
// ── Convenience result helpers ────────────────────────────────────────────────
/** Create a successful `PluginResult`. */
export function resultOk(output) {
    return { success: true, output };
}
/** Create a failure `PluginResult`. */
export function resultErr(error) {
    return { success: false, output: "", error };
}
/** Create a "continue" `MiddlewareAction` with the given data. */
export function middlewareContinue(data) {
    return { action: "continue", data };
}
/** Create a "block" `MiddlewareAction` with the given reason. */
export function middlewareBlock(reason) {
    return { action: "block", reason };
}
//# sourceMappingURL=types.js.map