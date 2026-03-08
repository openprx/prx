<script lang="ts">
	import { onMount } from 'svelte';
	import { RefreshCw, Save, Settings } from 'lucide-svelte';
	import ConfigField from '$lib/components/config/ConfigField.svelte';
	import {
		buildCategories,
		labelize,
		type ConfigCategory,
		type JsonSchema,
		type JsonValue
	} from '$lib/config/schema';

	type LoadState = {
		config: Record<string, JsonValue>;
		schema: JsonSchema;
		categories: ConfigCategory[];
	};

	let loading = $state(true);
	let saving = $state(false);
	let loadError = $state('');
	let saveError = $state('');
	let saveNotice = $state('');
	let config = $state<Record<string, JsonValue>>({});
	let schema = $state<JsonSchema>({});
	let categories = $state<ConfigCategory[]>([]);
	let activeCategoryId = $state('');

	const pageSubtitle =
		'Schema-driven config editor with category navigation. The form renders from /api/config/schema and saves back to /api/config.';

	let activeCategory = $derived(
		categories.find((category) => category.id === activeCategoryId) ?? categories[0]
	);

	async function loadConfigPage() {
		loading = true;
		loadError = '';
		saveError = '';
		saveNotice = '';

		try {
			const [configResponse, schemaResponse] = await Promise.all([
				fetch('/api/config'),
				fetch('/api/config/schema')
			]);

			if (!configResponse.ok) {
				throw new Error(`Failed to load config: ${configResponse.status}`);
			}

			if (!schemaResponse.ok) {
				throw new Error(`Failed to load schema: ${schemaResponse.status}`);
			}

			const nextState = {
				config: (await configResponse.json()) as Record<string, JsonValue>,
				schema: (await schemaResponse.json()) as JsonSchema,
				categories: [] as ConfigCategory[]
			} satisfies LoadState;

			nextState.categories = buildCategories(nextState.schema);
			config = nextState.config;
			schema = nextState.schema;
			categories = nextState.categories;

			if (!nextState.categories.some((category) => category.id === activeCategoryId)) {
				activeCategoryId = nextState.categories[0]?.id ?? '';
			}
		} catch (error) {
			loadError = error instanceof Error ? error.message : 'Failed to load config page.';
		} finally {
			loading = false;
		}
	}

	async function saveConfig() {
		saving = true;
		saveError = '';
		saveNotice = '';

		try {
			const response = await fetch('/api/config', {
				method: 'PUT',
				headers: {
					'content-type': 'application/json'
				},
				body: JSON.stringify(config)
			});

			const payload = (await response.json()) as { error?: string; restart_required?: boolean };
			if (!response.ok) {
				throw new Error(payload.error ?? `Save failed with status ${response.status}`);
			}

			saveNotice = payload.restart_required
				? 'Config saved. A restart is required for all changes to take effect.'
				: 'Config saved.';
		} catch (error) {
			saveError = error instanceof Error ? error.message : 'Failed to save config.';
		} finally {
			saving = false;
		}
	}

	function updateTopLevelProperty(key: string, value: JsonValue) {
		config = {
			...config,
			[key]: value
		};
	}

	onMount(loadConfigPage);
</script>

<div class="min-h-full space-y-6 bg-[radial-gradient(circle_at_top,_rgba(59,130,246,0.15),_transparent_38%),linear-gradient(180deg,_rgba(15,23,42,0.35),_transparent_45%)] p-1 text-slate-100">
	<section class="rounded-[28px] border border-white/8 bg-slate-950/55 p-6 shadow-[0_20px_60px_rgba(2,6,23,0.45)] backdrop-blur">
		<div class="flex flex-col gap-5 xl:flex-row xl:items-start xl:justify-between">
			<div class="space-y-3">
				<div class="inline-flex items-center gap-2 rounded-full border border-sky-400/20 bg-sky-400/10 px-3 py-1 text-[11px] font-medium uppercase tracking-[0.24em] text-sky-200">
					<Settings size={14} />
					Config
				</div>
				<div>
					<h2 class="text-2xl font-semibold tracking-tight text-white">Runtime Configuration</h2>
					<p class="mt-2 max-w-3xl text-sm leading-6 text-slate-400">{pageSubtitle}</p>
				</div>
			</div>
			<div class="flex flex-wrap items-center gap-3">
				<button
					type="button"
					class="inline-flex items-center gap-2 rounded-2xl border border-white/10 bg-white/[0.04] px-4 py-2.5 text-sm text-slate-200 transition hover:border-white/20 hover:bg-white/[0.08] hover:text-white"
					onclick={loadConfigPage}
					disabled={loading || saving}
				>
					<RefreshCw size={16} class={loading ? 'animate-spin' : ''} />
					Reload
				</button>
				<button
					type="button"
					class="inline-flex items-center gap-2 rounded-2xl bg-sky-500 px-4 py-2.5 text-sm font-medium text-slate-950 transition hover:bg-sky-400 disabled:cursor-not-allowed disabled:bg-sky-500/50"
					onclick={saveConfig}
					disabled={loading || saving}
				>
					<Save size={16} />
					{saving ? 'Saving...' : 'Save Config'}
				</button>
			</div>
		</div>

		{#if saveNotice}
			<div class="mt-5 rounded-2xl border border-emerald-400/20 bg-emerald-500/10 px-4 py-3 text-sm text-emerald-200">
				{saveNotice}
			</div>
		{/if}
		{#if saveError}
			<div class="mt-5 rounded-2xl border border-rose-400/20 bg-rose-500/10 px-4 py-3 text-sm text-rose-200">
				{saveError}
			</div>
		{/if}
		{#if loadError}
			<div class="mt-5 rounded-2xl border border-rose-400/20 bg-rose-500/10 px-4 py-3 text-sm text-rose-200">
				{loadError}
			</div>
		{/if}
	</section>

	{#if loading}
		<section class="rounded-[28px] border border-white/8 bg-slate-950/45 p-6 text-sm text-slate-400">
			Loading configuration schema...
		</section>
	{:else if activeCategory}
		<section class="grid gap-6 xl:grid-cols-[240px_minmax(0,1fr)]">
			<aside class="h-fit rounded-[28px] border border-white/8 bg-slate-950/45 p-4 backdrop-blur">
				<div class="mb-4 px-2">
					<h3 class="text-xs font-semibold uppercase tracking-[0.24em] text-slate-400">Sections</h3>
					<p class="mt-2 text-xs leading-5 text-slate-500">Categories are derived from top-level config keys.</p>
				</div>
				<nav class="space-y-2">
					{#each categories as category}
						<button
							type="button"
							class={`flex w-full items-center justify-between rounded-2xl px-3 py-3 text-left text-sm transition ${
								activeCategoryId === category.id
									? 'bg-sky-500/15 text-sky-100 ring-1 ring-inset ring-sky-400/30'
									: 'bg-white/[0.02] text-slate-300 hover:bg-white/[0.05] hover:text-white'
							}`}
							onclick={() => (activeCategoryId = category.id)}
						>
							<span class="font-medium">{category.title}</span>
							<span class="rounded-full bg-black/20 px-2 py-0.5 text-[11px] text-slate-400">
								{category.keys.length}
							</span>
						</button>
					{/each}
				</nav>
			</aside>

			<div class="space-y-4">
				<div class="rounded-[28px] border border-white/8 bg-slate-950/45 p-5 backdrop-blur">
					<div class="flex flex-wrap items-center justify-between gap-3">
						<div>
							<h3 class="text-lg font-semibold text-white">{activeCategory.title}</h3>
							<p class="mt-2 text-sm text-slate-400">
								Showing {activeCategory.keys.length} top-level section{activeCategory.keys.length === 1 ? '' : 's'} in this category.
							</p>
						</div>
						<div class="flex flex-wrap gap-2">
							{#each activeCategory.keys as key}
								<span class="rounded-full border border-white/10 bg-white/[0.03] px-3 py-1 text-xs text-slate-300">
									{labelize(key)}
								</span>
							{/each}
						</div>
					</div>
				</div>

				{#each activeCategory.keys as key}
					{#if schema.properties?.[key]}
						<ConfigField
							name={key}
							schema={schema.properties[key]}
							rootSchema={schema}
							value={config[key]}
							required={(schema.required ?? []).includes(key)}
							onChange={(value) => updateTopLevelProperty(key, value)}
						/>
					{/if}
				{/each}
			</div>
		</section>
	{:else}
		<section class="rounded-[28px] border border-white/8 bg-slate-950/45 p-6 text-sm text-slate-400">
			The config schema did not expose any editable top-level properties.
		</section>
	{/if}
</div>
