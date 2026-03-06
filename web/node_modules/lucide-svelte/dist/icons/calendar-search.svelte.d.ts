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
export type CalendarSearchProps = typeof __propDef.props;
export type CalendarSearchEvents = typeof __propDef.events;
export type CalendarSearchSlots = typeof __propDef.slots;
/**
 * @component @name CalendarSearch
 * @description Lucide SVG icon component, renders SVG Element with children.
 *
 * @preview ![img](data:image/svg+xml;base64,PHN2ZyAgeG1sbnM9Imh0dHA6Ly93d3cudzMub3JnLzIwMDAvc3ZnIgogIHdpZHRoPSIyNCIKICBoZWlnaHQ9IjI0IgogIHZpZXdCb3g9IjAgMCAyNCAyNCIKICBmaWxsPSJub25lIgogIHN0cm9rZT0iIzAwMCIgc3R5bGU9ImJhY2tncm91bmQtY29sb3I6ICNmZmY7IGJvcmRlci1yYWRpdXM6IDJweCIKICBzdHJva2Utd2lkdGg9IjIiCiAgc3Ryb2tlLWxpbmVjYXA9InJvdW5kIgogIHN0cm9rZS1saW5lam9pbj0icm91bmQiCj4KICA8cGF0aCBkPSJNMTYgMnY0IiAvPgogIDxwYXRoIGQ9Ik0yMSAxMS43NVY2YTIgMiAwIDAgMC0yLTJINWEyIDIgMCAwIDAtMiAydjE0YTIgMiAwIDAgMCAyIDJoNy4yNSIgLz4KICA8cGF0aCBkPSJtMjIgMjItMS44NzUtMS44NzUiIC8+CiAgPHBhdGggZD0iTTMgMTBoMTgiIC8+CiAgPHBhdGggZD0iTTggMnY0IiAvPgogIDxjaXJjbGUgY3g9IjE4IiBjeT0iMTgiIHI9IjMiIC8+Cjwvc3ZnPgo=) - https://lucide.dev/icons/calendar-search
 * @see https://lucide.dev/guide/packages/lucide-svelte - Documentation
 *
 * @param {Object} props - Lucide icons props and any valid SVG attribute
 * @returns {FunctionalComponent} Svelte component
 *
 */
export default class CalendarSearch extends SvelteComponentTyped<CalendarSearchProps, CalendarSearchEvents, CalendarSearchSlots> {
}
export {};
