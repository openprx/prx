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
export type ToiletProps = typeof __propDef.props;
export type ToiletEvents = typeof __propDef.events;
export type ToiletSlots = typeof __propDef.slots;
/**
 * @component @name Toilet
 * @description Lucide SVG icon component, renders SVG Element with children.
 *
 * @preview ![img](data:image/svg+xml;base64,PHN2ZyAgeG1sbnM9Imh0dHA6Ly93d3cudzMub3JnLzIwMDAvc3ZnIgogIHdpZHRoPSIyNCIKICBoZWlnaHQ9IjI0IgogIHZpZXdCb3g9IjAgMCAyNCAyNCIKICBmaWxsPSJub25lIgogIHN0cm9rZT0iIzAwMCIgc3R5bGU9ImJhY2tncm91bmQtY29sb3I6ICNmZmY7IGJvcmRlci1yYWRpdXM6IDJweCIKICBzdHJva2Utd2lkdGg9IjIiCiAgc3Ryb2tlLWxpbmVjYXA9InJvdW5kIgogIHN0cm9rZS1saW5lam9pbj0icm91bmQiCj4KICA8cGF0aCBkPSJNNyAxMmgxM2ExIDEgMCAwIDEgMSAxIDUgNSAwIDAgMS01IDVoLS41OThhLjUuNSAwIDAgMC0uNDI0Ljc2NWwxLjU0NCAyLjQ3YS41LjUgMCAwIDEtLjQyNC43NjVINS40MDJhLjUuNSAwIDAgMS0uNDI0LS43NjVMNyAxOCIgLz4KICA8cGF0aCBkPSJNOCAxOGE1IDUgMCAwIDEtNS01VjRhMiAyIDAgMCAxIDItMmg4YTIgMiAwIDAgMSAyIDJ2OCIgLz4KPC9zdmc+) - https://lucide.dev/icons/toilet
 * @see https://lucide.dev/guide/packages/lucide-svelte - Documentation
 *
 * @param {Object} props - Lucide icons props and any valid SVG attribute
 * @returns {FunctionalComponent} Svelte component
 *
 */
export default class Toilet extends SvelteComponentTyped<ToiletProps, ToiletEvents, ToiletSlots> {
}
export {};
