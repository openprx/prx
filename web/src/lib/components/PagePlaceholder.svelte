<script lang="ts">
	import type { Component } from 'svelte';

	interface Props {
		title: string;
		icon: Component;
	}

	let { title, icon }: Props = $props();
</script>

<div class="placeholder">
	<div class="placeholder-icon">
		{@const Icon = icon}
		<Icon size={32} strokeWidth={1.2} />
	</div>
	<h2>{title}</h2>
	<p>Coming soon</p>
</div>

<style>
	.placeholder {
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		height: 60vh;
		color: var(--text-muted);
		gap: 12px;
	}

	.placeholder-icon {
		opacity: 0.4;
	}

	h2 {
		font-size: 18px;
		font-weight: 500;
		color: var(--text-secondary);
	}

	p {
		font-size: 13px;
	}
</style>
