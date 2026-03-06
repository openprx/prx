<script>/**
 * @license lucide-svelte v0.469.0 - ISC
 *
 * This source code is licensed under the ISC license.
 * See the LICENSE file in the root directory of this source tree.
 */
import Icon from '../Icon.svelte';
const iconNode = [["path", { "d": "M16 10h2" }], ["path", { "d": "M16 14h2" }], ["path", { "d": "M6.17 15a3 3 0 0 1 5.66 0" }], ["circle", { "cx": "9", "cy": "11", "r": "2" }], ["rect", { "x": "2", "y": "5", "width": "20", "height": "14", "rx": "2" }]];
/**
 * @component @name IdCard
 * @description Lucide SVG icon component, renders SVG Element with children.
 *
 * @preview ![img](data:image/svg+xml;base64,PHN2ZyAgeG1sbnM9Imh0dHA6Ly93d3cudzMub3JnLzIwMDAvc3ZnIgogIHdpZHRoPSIyNCIKICBoZWlnaHQ9IjI0IgogIHZpZXdCb3g9IjAgMCAyNCAyNCIKICBmaWxsPSJub25lIgogIHN0cm9rZT0iIzAwMCIgc3R5bGU9ImJhY2tncm91bmQtY29sb3I6ICNmZmY7IGJvcmRlci1yYWRpdXM6IDJweCIKICBzdHJva2Utd2lkdGg9IjIiCiAgc3Ryb2tlLWxpbmVjYXA9InJvdW5kIgogIHN0cm9rZS1saW5lam9pbj0icm91bmQiCj4KICA8cGF0aCBkPSJNMTYgMTBoMiIgLz4KICA8cGF0aCBkPSJNMTYgMTRoMiIgLz4KICA8cGF0aCBkPSJNNi4xNyAxNWEzIDMgMCAwIDEgNS42NiAwIiAvPgogIDxjaXJjbGUgY3g9IjkiIGN5PSIxMSIgcj0iMiIgLz4KICA8cmVjdCB4PSIyIiB5PSI1IiB3aWR0aD0iMjAiIGhlaWdodD0iMTQiIHJ4PSIyIiAvPgo8L3N2Zz4K) - https://lucide.dev/icons/id-card
 * @see https://lucide.dev/guide/packages/lucide-svelte - Documentation
 *
 * @param {Object} props - Lucide icons props and any valid SVG attribute
 * @returns {FunctionalComponent} Svelte component
 *
 */
</script>

<Icon name="id-card" {...$$props} iconNode={iconNode}>
  <slot/>
</Icon>
