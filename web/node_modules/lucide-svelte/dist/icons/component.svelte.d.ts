/**
 * @license lucide-svelte v0.469.0 - ISC
 *
 * This source code is licensed under the ISC license.
 * See the LICENSE file in the root directory of this source tree.
 */
import { SvelteComponentTyped } from "svelte";
import type { IconProps } from '../types.js';
declare const __propDef: {
    props: IconProps;
    events: {
        [evt: string]: CustomEvent<any>;
    };
    slots: {
        default: {};
    };
};
export type ComponentProps = typeof __propDef.props;
export type ComponentEvents = typeof __propDef.events;
export type ComponentSlots = typeof __propDef.slots;
/**
 * @component @name Component
 * @description Lucide SVG icon component, renders SVG Element with children.
 *
 * @preview ![img](data:image/svg+xml;base64,PHN2ZyAgeG1sbnM9Imh0dHA6Ly93d3cudzMub3JnLzIwMDAvc3ZnIgogIHdpZHRoPSIyNCIKICBoZWlnaHQ9IjI0IgogIHZpZXdCb3g9IjAgMCAyNCAyNCIKICBmaWxsPSJub25lIgogIHN0cm9rZT0iIzAwMCIgc3R5bGU9ImJhY2tncm91bmQtY29sb3I6ICNmZmY7IGJvcmRlci1yYWRpdXM6IDJweCIKICBzdHJva2Utd2lkdGg9IjIiCiAgc3Ryb2tlLWxpbmVjYXA9InJvdW5kIgogIHN0cm9rZS1saW5lam9pbj0icm91bmQiCj4KICA8cGF0aCBkPSJNMTUuNTM2IDExLjI5M2ExIDEgMCAwIDAgMCAxLjQxNGwyLjM3NiAyLjM3N2ExIDEgMCAwIDAgMS40MTQgMGwyLjM3Ny0yLjM3N2ExIDEgMCAwIDAgMC0xLjQxNGwtMi4zNzctMi4zNzdhMSAxIDAgMCAwLTEuNDE0IDB6IiAvPgogIDxwYXRoIGQ9Ik0yLjI5NyAxMS4yOTNhMSAxIDAgMCAwIDAgMS40MTRsMi4zNzcgMi4zNzdhMSAxIDAgMCAwIDEuNDE0IDBsMi4zNzctMi4zNzdhMSAxIDAgMCAwIDAtMS40MTRMNi4wODggOC45MTZhMSAxIDAgMCAwLTEuNDE0IDB6IiAvPgogIDxwYXRoIGQ9Ik04LjkxNiAxNy45MTJhMSAxIDAgMCAwIDAgMS40MTVsMi4zNzcgMi4zNzZhMSAxIDAgMCAwIDEuNDE0IDBsMi4zNzctMi4zNzZhMSAxIDAgMCAwIDAtMS40MTVsLTIuMzc3LTIuMzc2YTEgMSAwIDAgMC0xLjQxNCAweiIgLz4KICA8cGF0aCBkPSJNOC45MTYgNC42NzRhMSAxIDAgMCAwIDAgMS40MTRsMi4zNzcgMi4zNzZhMSAxIDAgMCAwIDEuNDE0IDBsMi4zNzctMi4zNzZhMSAxIDAgMCAwIDAtMS40MTRsLTIuMzc3LTIuMzc3YTEgMSAwIDAgMC0xLjQxNCAweiIgLz4KPC9zdmc+Cg==) - https://lucide.dev/icons/component
 * @see https://lucide.dev/guide/packages/lucide-svelte - Documentation
 *
 * @param {Object} props - Lucide icons props and any valid SVG attribute
 * @returns {FunctionalComponent} Svelte component
 *
 */
export default class Component extends SvelteComponentTyped<ComponentProps, ComponentEvents, ComponentSlots> {
}
export {};
