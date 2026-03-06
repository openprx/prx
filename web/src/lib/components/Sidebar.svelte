<script lang="ts">
	import { page } from '$app/stores';
	import { sidebarCollapsed, toggleSidebar } from '$lib/stores/sidebar';
	import {
		LayoutDashboard,
		Webhook,
		Puzzle,
		Sparkles,
		Monitor,
		Clock,
		Brain,
		Network,
		Settings,
		PanelLeftClose,
		PanelLeftOpen
	} from 'lucide-svelte';

	const navItems = [
		{ href: '/', label: 'Dashboard', icon: LayoutDashboard },
		{ href: '/hooks', label: 'Hooks', icon: Webhook },
		{ href: '/mcp', label: 'MCP', icon: Puzzle },
		{ href: '/skills', label: 'Skills', icon: Sparkles },
		{ href: '/sessions', label: 'Sessions', icon: Monitor },
		{ href: '/cron', label: 'Cron', icon: Clock },
		{ href: '/memory', label: 'Memory', icon: Brain },
		{ href: '/nodes', label: 'Nodes', icon: Network },
		{ href: '/config', label: 'Config', icon: Settings }
	];

	function isActive(href: string, pathname: string): boolean {
		if (href === '/') return pathname === '/';
		return pathname.startsWith(href);
	}
</script>

<aside
	class="sidebar"
	class:collapsed={$sidebarCollapsed}
>
	<div class="sidebar-header">
		{#if !$sidebarCollapsed}
			<span class="sidebar-title">OpenPRX</span>
		{/if}
		<button class="toggle-btn" onclick={toggleSidebar} aria-label="Toggle sidebar">
			{#if $sidebarCollapsed}
				<PanelLeftOpen size={18} />
			{:else}
				<PanelLeftClose size={18} />
			{/if}
		</button>
	</div>

	<nav class="sidebar-nav">
		{#each navItems as item}
			<a
				href={item.href}
				class="nav-item"
				class:active={isActive(item.href, $page.url.pathname)}
				title={$sidebarCollapsed ? item.label : ''}
			>
				{@const Icon = item.icon}
				<Icon size={18} strokeWidth={1.8} />
				{#if !$sidebarCollapsed}
					<span class="nav-label">{item.label}</span>
				{/if}
			</a>
		{/each}
	</nav>

	<div class="sidebar-footer">
		{#if !$sidebarCollapsed}
			<span class="version">v0.1.0</span>
		{/if}
	</div>
</aside>

<style>
	.sidebar {
		width: 220px;
		min-width: 220px;
		height: 100vh;
		background: var(--bg-secondary);
		border-right: 1px solid var(--border-color);
		display: flex;
		flex-direction: column;
		transition: width 0.2s ease, min-width 0.2s ease;
		overflow: hidden;
	}

	.sidebar.collapsed {
		width: 52px;
		min-width: 52px;
	}

	.sidebar-header {
		display: flex;
		align-items: center;
		justify-content: space-between;
		padding: 16px 12px;
		border-bottom: 1px solid var(--border-color);
		min-height: 56px;
	}

	.sidebar-title {
		font-size: 15px;
		font-weight: 600;
		color: var(--text-primary);
		white-space: nowrap;
		letter-spacing: -0.01em;
	}

	.toggle-btn {
		background: none;
		border: none;
		color: var(--text-secondary);
		cursor: pointer;
		padding: 4px;
		border-radius: 4px;
		display: flex;
		align-items: center;
		justify-content: center;
		transition: color 0.15s;
	}

	.toggle-btn:hover {
		color: var(--text-primary);
	}

	.sidebar-nav {
		flex: 1;
		padding: 8px 8px;
		display: flex;
		flex-direction: column;
		gap: 2px;
		overflow-y: auto;
	}

	.nav-item {
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 8px 10px;
		border-radius: 6px;
		color: var(--text-secondary);
		text-decoration: none;
		font-size: 13px;
		font-weight: 450;
		transition: background 0.15s, color 0.15s;
		white-space: nowrap;
	}

	.nav-item:hover {
		background: var(--bg-hover);
		color: var(--text-primary);
	}

	.nav-item.active {
		background: var(--bg-hover);
		color: var(--accent);
	}

	.nav-label {
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.sidebar-footer {
		padding: 12px;
		border-top: 1px solid var(--border-color);
		min-height: 44px;
	}

	.version {
		font-size: 11px;
		color: var(--text-muted);
	}
</style>
