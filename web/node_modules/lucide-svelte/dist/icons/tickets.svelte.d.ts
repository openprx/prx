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
export type TicketsProps = typeof __propDef.props;
export type TicketsEvents = typeof __propDef.events;
export type TicketsSlots = typeof __propDef.slots;
/**
 * @component @name Tickets
 * @description Lucide SVG icon component, renders SVG Element with children.
 *
 * @preview ![img](data:image/svg+xml;base64,PHN2ZyAgeG1sbnM9Imh0dHA6Ly93d3cudzMub3JnLzIwMDAvc3ZnIgogIHdpZHRoPSIyNCIKICBoZWlnaHQ9IjI0IgogIHZpZXdCb3g9IjAgMCAyNCAyNCIKICBmaWxsPSJub25lIgogIHN0cm9rZT0iIzAwMCIgc3R5bGU9ImJhY2tncm91bmQtY29sb3I6ICNmZmY7IGJvcmRlci1yYWRpdXM6IDJweCIKICBzdHJva2Utd2lkdGg9IjIiCiAgc3Ryb2tlLWxpbmVjYXA9InJvdW5kIgogIHN0cm9rZS1saW5lam9pbj0icm91bmQiCj4KICA8cGF0aCBkPSJtNC41IDggMTAuNTgtNS4wNmExIDEgMCAwIDEgMS4zNDIuNDg4TDE4LjUgOCIgLz4KICA8cGF0aCBkPSJNNiAxMFY4IiAvPgogIDxwYXRoIGQ9Ik02IDE0djEiIC8+CiAgPHBhdGggZD0iTTYgMTl2MiIgLz4KICA8cmVjdCB4PSIyIiB5PSI4IiB3aWR0aD0iMjAiIGhlaWdodD0iMTMiIHJ4PSIyIiAvPgo8L3N2Zz4K) - https://lucide.dev/icons/tickets
 * @see https://lucide.dev/guide/packages/lucide-svelte - Documentation
 *
 * @param {Object} props - Lucide icons props and any valid SVG attribute
 * @returns {FunctionalComponent} Svelte component
 *
 */
export default class Tickets extends SvelteComponentTyped<TicketsProps, TicketsEvents, TicketsSlots> {
}
export {};
