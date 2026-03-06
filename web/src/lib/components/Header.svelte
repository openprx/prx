<script lang="ts">
	import { page } from '$app/stores';

	const titles: Record<string, string> = {
		'/': 'Dashboard',
		'/hooks': 'Hooks',
		'/mcp': 'MCP Servers',
		'/skills': 'Skills',
		'/sessions': 'Sessions',
		'/cron': 'Cron Jobs',
		'/memory': 'Memory',
		'/nodes': 'Nodes',
		'/config': 'Configuration'
	};

	function getTitle(pathname: string): string {
		return titles[pathname] || 'OpenPRX Console';
	}
</script>

<header class="header">
	<div class="header-left">
		<h1 class="page-title">{getTitle($page.url.pathname)}</h1>
	</div>
	<div class="header-right">
		<span class="status-badge">Running</span>
	</div>
</header>

<style>
	.header {
		height: 48px;
		min-height: 48px;
		background: var(--bg-secondary);
		border-bottom: 1px solid var(--border-color);
		display: flex;
		align-items: center;
		justify-content: space-between;
		padding: 0 20px;
	}

	.header-left {
		display: flex;
		align-items: center;
		gap: 12px;
	}

	.page-title {
		font-size: 14px;
		font-weight: 600;
		color: var(--text-primary);
	}

	.header-right {
		display: flex;
		align-items: center;
		gap: 12px;
	}

	.status-badge {
		font-size: 11px;
		padding: 3px 8px;
		border-radius: 4px;
		background: rgba(34, 197, 94, 0.12);
		color: var(--success);
		font-weight: 500;
	}
</style>
