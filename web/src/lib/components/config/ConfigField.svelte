<script lang="ts">
	import ConfigField from './ConfigField.svelte';
	import {
		inferDefaultValue,
		isLongTextField,
		isNullableSchema,
		isSchemaMap,
		isSensitivePath,
		labelize,
		resolveSchema,
		type JsonSchema,
		type JsonValue,
		type PathSegment
	} from '$lib/config/schema';

	type Props = {
		name: string;
		schema: JsonSchema;
		rootSchema: JsonSchema;
		value: JsonValue | undefined;
		path?: PathSegment[];
		required?: boolean;
		depth?: number;
		onChange: (nextValue: JsonValue) => void;
		onDelete?: () => void;
	};

	let {
		name,
		schema,
		rootSchema,
		value,
		path = [],
		required = false,
		depth = 0,
		onChange,
		onDelete
	}: Props = $props();

	let newMapKey = $state('');

	let normalizedSchema = $derived(resolveSchema(schema, rootSchema));
	let fieldPath = $derived([...path, name]);
	let fieldLabel = $derived(normalizedSchema.title || labelize(name));
	let isNullable = $derived(isNullableSchema(schema));
	let stringValue = $derived(typeof value === 'string' ? value : '');
	let objectValue = $derived(
		value && typeof value === 'object' && !Array.isArray(value)
			? (value as Record<string, JsonValue>)
			: {}
	);
	let arrayValue = $derived(Array.isArray(value) ? value : []);
	let isMap = $derived(isSchemaMap(normalizedSchema));
	let knownProperties = $derived(new Set(Object.keys(normalizedSchema.properties ?? {})));
	let mapEntries = $derived(
		Object.entries(objectValue).filter(([key]) => !knownProperties.has(key))
	);
	let itemSchema = $derived(
		Array.isArray(normalizedSchema.items) ? normalizedSchema.items[0] : normalizedSchema.items
	);

	function updateObjectProperty(key: string, nextValue: JsonValue) {
		onChange({
			...objectValue,
			[key]: nextValue
		});
	}

	function removeObjectProperty(key: string) {
		const nextValue = { ...objectValue };
		delete nextValue[key];
		onChange(nextValue);
	}

	function updateArrayItem(index: number, nextValue: JsonValue) {
		const nextValueList = [...arrayValue];
		nextValueList[index] = nextValue;
		onChange(nextValueList);
	}

	function removeArrayItem(index: number) {
		onChange(arrayValue.filter((_, itemIndex) => itemIndex !== index));
	}

	function addArrayItem() {
		onChange([...arrayValue, inferDefaultValue(itemSchema, rootSchema)]);
	}

	function enableSection() {
		onChange(inferDefaultValue(normalizedSchema, rootSchema));
	}

	function clearValue() {
		onChange(null);
	}

	function addMapEntry() {
		const key = newMapKey.trim();
		if (!key || key in objectValue) {
			return;
		}
		const additionalSchema =
			normalizedSchema.additionalProperties && normalizedSchema.additionalProperties !== true
				? normalizedSchema.additionalProperties
				: undefined;
		onChange({
			...objectValue,
			[key]: inferDefaultValue(additionalSchema, rootSchema)
		});
		newMapKey = '';
	}

	function onBooleanChange(event: Event) {
		const target = event.currentTarget as HTMLInputElement;
		onChange(target.checked);
	}

	function onNumberChange(event: Event) {
		const target = event.currentTarget as HTMLInputElement;
		onChange(target.value === '' ? null : Number(target.value));
	}

	function onStringChange(event: Event) {
		const target = event.currentTarget as HTMLInputElement | HTMLTextAreaElement | HTMLSelectElement;
		onChange(target.value);
	}
</script>

<div class={`rounded-2xl border border-white/8 bg-white/[0.03] ${depth === 0 ? 'p-5' : 'p-4'}`}>
	<div class="mb-4 flex flex-wrap items-start justify-between gap-3">
		<div class="space-y-1">
			<div class="flex items-center gap-2">
				<h3 class="text-sm font-semibold text-slate-100">{fieldLabel}</h3>
				{#if required}
					<span class="rounded-full border border-sky-400/30 bg-sky-400/10 px-2 py-0.5 text-[10px] font-medium uppercase tracking-[0.18em] text-sky-200">
						Required
					</span>
				{/if}
			</div>
			{#if normalizedSchema.description}
				<p class="max-w-3xl text-xs leading-5 text-slate-400">{normalizedSchema.description}</p>
			{/if}
		</div>
		<div class="flex items-center gap-2">
			{#if isNullable && value !== null && value !== undefined}
				<button
					type="button"
					class="rounded-xl border border-white/10 px-3 py-1.5 text-xs text-slate-300 transition hover:border-white/20 hover:bg-white/5 hover:text-white"
					onclick={clearValue}
				>
					Clear
				</button>
			{/if}
			{#if onDelete}
				<button
					type="button"
					class="rounded-xl border border-rose-400/20 px-3 py-1.5 text-xs text-rose-200 transition hover:border-rose-400/40 hover:bg-rose-500/10"
					onclick={onDelete}
				>
					Remove
				</button>
			{/if}
		</div>
	</div>

	{#if value === null || value === undefined}
		{#if normalizedSchema.type === 'object' || normalizedSchema.properties || isMap || normalizedSchema.type === 'array'}
			<button
				type="button"
				class="rounded-xl border border-dashed border-white/12 px-3 py-2 text-sm text-slate-300 transition hover:border-white/20 hover:bg-white/5 hover:text-white"
				onclick={enableSection}
			>
				{normalizedSchema.type === 'array' ? 'Initialize list' : 'Enable section'}
			</button>
		{:else}
			<div class="text-sm text-slate-500">Unset</div>
		{/if}
	{:else if normalizedSchema.type === 'object' || normalizedSchema.properties || isMap}
		<div class="space-y-4">
			{#each Object.entries(normalizedSchema.properties ?? {}) as [propertyName, propertySchema]}
				<ConfigField
					name={propertyName}
					schema={propertySchema}
					rootSchema={rootSchema}
					value={objectValue[propertyName]}
					path={fieldPath}
					required={(normalizedSchema.required ?? []).includes(propertyName)}
					depth={depth + 1}
					onChange={(nextValue) => updateObjectProperty(propertyName, nextValue)}
				/>
			{/each}

			{#if isMap}
				<div class="rounded-2xl border border-dashed border-white/10 bg-black/10 p-4">
					<div class="mb-3 flex flex-wrap items-center justify-between gap-3">
						<div>
							<h4 class="text-xs font-semibold uppercase tracking-[0.22em] text-slate-300">Entries</h4>
							<p class="mt-1 text-xs text-slate-500">Map keys are created dynamically.</p>
						</div>
						<div class="flex flex-wrap items-center gap-2">
							<input
								bind:value={newMapKey}
								class="rounded-xl border border-white/10 bg-slate-950/60 px-3 py-2 text-sm text-slate-100 outline-none transition focus:border-sky-400/50"
								placeholder="New key"
							/>
							<button
								type="button"
								class="rounded-xl bg-sky-500 px-3 py-2 text-sm font-medium text-slate-950 transition hover:bg-sky-400"
								onclick={addMapEntry}
							>
								Add entry
							</button>
						</div>
					</div>

					<div class="space-y-3">
						{#if !mapEntries.length}
							<div class="text-sm text-slate-500">No entries.</div>
						{/if}
						{#each mapEntries as [entryName, entryValue]}
							<ConfigField
								name={entryName}
								schema={normalizedSchema.additionalProperties === true ? {} : normalizedSchema.additionalProperties || {}}
								rootSchema={rootSchema}
								value={entryValue}
								path={fieldPath}
								depth={depth + 1}
								onChange={(nextValue) => updateObjectProperty(entryName, nextValue)}
								onDelete={() => removeObjectProperty(entryName)}
							/>
						{/each}
					</div>
				</div>
			{/if}
		</div>
	{:else if normalizedSchema.type === 'array'}
		<div class="space-y-3">
			{#if !arrayValue.length}
				<div class="text-sm text-slate-500">No items.</div>
			{/if}
			{#each arrayValue as itemValue, index}
				<ConfigField
					name={`${labelize(name)} ${index + 1}`}
					schema={itemSchema || {}}
					rootSchema={rootSchema}
					value={itemValue}
					path={[...fieldPath, index]}
					depth={depth + 1}
					onChange={(nextValue) => updateArrayItem(index, nextValue)}
					onDelete={() => removeArrayItem(index)}
				/>
			{/each}
			<button
				type="button"
				class="rounded-xl border border-dashed border-white/12 px-3 py-2 text-sm text-slate-300 transition hover:border-white/20 hover:bg-white/5 hover:text-white"
				onclick={addArrayItem}
			>
				Add item
			</button>
		</div>
	{:else if normalizedSchema.enum?.length}
		<select
			class="w-full rounded-xl border border-white/10 bg-slate-950/60 px-3 py-2.5 text-sm text-slate-100 outline-none transition focus:border-sky-400/50"
			value={stringValue}
			onchange={onStringChange}
		>
			{#each normalizedSchema.enum as option}
				<option value={String(option)}>{String(option)}</option>
			{/each}
		</select>
	{:else if normalizedSchema.type === 'boolean'}
		<label class="inline-flex items-center gap-3 rounded-xl border border-white/10 bg-slate-950/40 px-3 py-2">
			<input
				type="checkbox"
				checked={Boolean(value)}
				class="h-4 w-4 rounded border-white/20 bg-slate-900 text-sky-500"
				onchange={onBooleanChange}
			/>
			<span class="text-sm text-slate-200">{Boolean(value) ? 'Enabled' : 'Disabled'}</span>
		</label>
	{:else if normalizedSchema.type === 'number' || normalizedSchema.type === 'integer'}
		<input
			type="number"
			value={typeof value === 'number' ? value : ''}
			class="w-full rounded-xl border border-white/10 bg-slate-950/60 px-3 py-2.5 text-sm text-slate-100 outline-none transition focus:border-sky-400/50"
			oninput={onNumberChange}
		/>
	{:else if normalizedSchema.type === 'string' && isLongTextField(fieldPath, normalizedSchema)}
		<textarea
			rows="4"
			class="min-h-28 w-full rounded-xl border border-white/10 bg-slate-950/60 px-3 py-2.5 text-sm text-slate-100 outline-none transition focus:border-sky-400/50"
			value={stringValue}
			oninput={onStringChange}
		></textarea>
	{:else}
		<input
			type={isSensitivePath(fieldPath) ? 'password' : 'text'}
			value={stringValue}
			class="w-full rounded-xl border border-white/10 bg-slate-950/60 px-3 py-2.5 text-sm text-slate-100 outline-none transition focus:border-sky-400/50"
			oninput={onStringChange}
		/>
	{/if}
</div>
