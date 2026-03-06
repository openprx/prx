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
export type ListOrderedProps = typeof __propDef.props;
export type ListOrderedEvents = typeof __propDef.events;
export type ListOrderedSlots = typeof __propDef.slots;
/**
 * @component @name ListOrdered
 * @description Lucide SVG icon component, renders SVG Element with children.
 *
 * @preview ![img](data:image/svg+xml;base64,PHN2ZyAgeG1sbnM9Imh0dHA6Ly93d3cudzMub3JnLzIwMDAvc3ZnIgogIHdpZHRoPSIyNCIKICBoZWlnaHQ9IjI0IgogIHZpZXdCb3g9IjAgMCAyNCAyNCIKICBmaWxsPSJub25lIgogIHN0cm9rZT0iIzAwMCIgc3R5bGU9ImJhY2tncm91bmQtY29sb3I6ICNmZmY7IGJvcmRlci1yYWRpdXM6IDJweCIKICBzdHJva2Utd2lkdGg9IjIiCiAgc3Ryb2tlLWxpbmVjYXA9InJvdW5kIgogIHN0cm9rZS1saW5lam9pbj0icm91bmQiCj4KICA8cGF0aCBkPSJNMTAgMTJoMTEiIC8+CiAgPHBhdGggZD0iTTEwIDE4aDExIiAvPgogIDxwYXRoIGQ9Ik0xMCA2aDExIiAvPgogIDxwYXRoIGQ9Ik00IDEwaDIiIC8+CiAgPHBhdGggZD0iTTQgNmgxdjQiIC8+CiAgPHBhdGggZD0iTTYgMThINGMwLTEgMi0yIDItM3MtMS0xLjUtMi0xIiAvPgo8L3N2Zz4K) - https://lucide.dev/icons/list-ordered
 * @see https://lucide.dev/guide/packages/lucide-svelte - Documentation
 *
 * @param {Object} props - Lucide icons props and any valid SVG attribute
 * @returns {FunctionalComponent} Svelte component
 *
 */
export default class ListOrdered extends SvelteComponentTyped<ListOrderedProps, ListOrderedEvents, ListOrderedSlots> {
}
export {};
