<script lang="ts">
	import '../app.css';
	import Sidebar from '$lib/components/Sidebar.svelte';
	import Header from '$lib/components/Header.svelte';
	import { sidebarCollapsed } from '$lib/stores/sidebar';

	let { children } = $props();
</script>

<div class="app-shell">
	<Sidebar />
	<div class="main-area">
		<Header />
		<main class="content" class:expanded={$sidebarCollapsed}>
			{@render children()}
		</main>
	</div>
</div>

<style>
	.app-shell {
		display: flex;
		height: 100vh;
		overflow: hidden;
	}

	.main-area {
		flex: 1;
		display: flex;
		flex-direction: column;
		overflow: hidden;
	}

	.content {
		flex: 1;
		overflow-y: auto;
		padding: 20px;
	}
</style>
