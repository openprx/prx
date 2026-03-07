/**
 * @prx/pdk — PRX WASM Plugin Development Kit for JavaScript/TypeScript
 *
 * ## Quick Start
 *
 * ```typescript
 * import { log, config, kv, events, http, clock, memory } from "@prx/pdk";
 * import type { ToolSpec, PluginResult } from "@prx/pdk";
 * import { resultOk, resultErr } from "@prx/pdk";
 *
 * // Implement a tool plugin
 * export function getSpec(): ToolSpec {
 *   return {
 *     name: "my_tool",
 *     description: "Does something useful",
 *     parametersSchema: JSON.stringify({
 *       type: "object",
 *       properties: { input: { type: "string" } },
 *       required: ["input"],
 *     }),
 *   };
 * }
 *
 * export function execute(argsJson: string): PluginResult {
 *   const args = JSON.parse(argsJson);
 *   log.info(`Processing: ${args.input}`);
 *   return resultOk(`Result: ${args.input}`);
 * }
 * ```
 *
 * ## Build
 *
 * ```sh
 * # Install build tools (once)
 * npm install --save-dev @bytecodealliance/jco @bytecodealliance/componentize-js
 *
 * # Compile TypeScript
 * npx tsc
 *
 * # Componentize to WASM
 * npx jco componentize dist/plugin.js --wit ../../../../wit --world tool -o plugin.wasm
 * ```
 */
// ── Convenience result helpers ────────────────────────────────────────────────
export { resultOk, resultErr, middlewareContinue, middlewareBlock, } from "./types.js";
// ── Host function wrappers ────────────────────────────────────────────────────
export { log, config, kv, events, http, clock, memory } from "./host.js";
//# sourceMappingURL=index.js.map