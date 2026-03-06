import { writable } from 'svelte/store';

export const sidebarCollapsed = writable(false);

export function toggleSidebar() {
	sidebarCollapsed.update((v) => !v);
}
