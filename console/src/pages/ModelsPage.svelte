<script>
  import { onDestroy, untrack } from 'svelte';
  import { api } from '../lib/api';
  import { ChangeFlow, isMasked, unmasked } from '../lib/change-flow.svelte.js';
  import ErrorNotice from '../lib/ErrorNotice.svelte';
  import PlanReview from '../lib/PlanReview.svelte';
  import { t } from '../lib/i18n';

  const OPERATIONS = [
    'chat',
    'embedding',
    'decision_endpoint',
    'rerank',
    'image_generation',
    'audio_generation',
    'video_generation'
  ];
  const FEATURES = ['native_tools', 'vision_input', 'streaming', 'structured_output'];
  const SCHEMA_DIALECTS = ['raw', 'no_root_combinators'];
  const CALL_DIALECTS = ['native', 'xml_function_tags'];

  let settingsCatalog = $state(null);
  let settingsLevel = $state('');
  let parameterValues = $state({});
  let settingsError = $state(null);
  let settingsRequest = 0;

  async function editSettings(entry) {
    const request = ++settingsRequest;
    settingsCatalog = null;
    settingsError = null;
    try {
      const catalog = await api.getModelSettings(entry.provider, entry.model);
      if (!alive || request !== settingsRequest) return;
      settingsCatalog = catalog;
      settingsLevel = catalog.settings?.level ?? '';
      parameterValues = Object.fromEntries(Object.entries(catalog.settings ?? {}).filter(([key]) => key !== 'level'));
    } catch (error) {
      if (alive && request === settingsRequest) settingsError = error;
    }
  }

  function planSettings() {
    if (!settingsCatalog) return;
    const settings = {};
    if (settingsLevel) settings.level = settingsLevel;
    for (const [name, value] of Object.entries(parameterValues)) {
      if (value === '' || value == null) continue;
      settings[name] = settingsCatalog.parameters[name].kind === 'range' ? Number(value) : value;
    }
    flow.preview({kind: 'set_model_settings', model: settingsCatalog.model, settings});
  }

  let models = $state([]);
  let loading = $state(true);
  let loadError = $state(null);

  let filterProvider = $state('');
  let filterOperation = $state('');
  let filterExecutable = $state(false);

  // A `[[providers.models]]` declaration. Every field but the provider is
  // optional and declares nothing when left unset.
  let formProvider = $state('');
  let formModel = $state('');
  let formOperations = $state([]);
  let formFeatures = $state(Object.fromEntries(FEATURES.map((feature) => [feature, ''])));
  let formSchemaDialect = $state('');
  let formCallDialect = $state('');
  let formContextWindow = $state('');
  // A masked provider or model was left out of the form.
  let maskedPrefill = $state(false);

  let alive = true;
  let modelRequest = 0;
  const flow = new ChangeFlow(afterApply);
  onDestroy(() => { alive = false; flow.dispose(); });

  // After an apply (or a stale plan) the masked-value warning no longer
  // describes the form.
  async function afterApply() {
    maskedPrefill = false;
    await loadModels();
  }

  async function loadModels() {
    if (!alive) return;
    const request = ++modelRequest;
    loading = true;
    try {
      const response = await api.getModels({
        provider: filterProvider.trim(),
        operation: filterOperation,
        executable: filterExecutable
      });
      if (!alive || request !== modelRequest) return;
      models = Array.isArray(response?.models) ? response.models : [];
      loadError = null;
    } catch (e) {
      if (!alive || request !== modelRequest) return;
      models = [];
      loadError = e;
    } finally {
      if (alive && request === modelRequest) loading = false;
    }
  }

  function edit(entry) {
    formProvider = unmasked(entry.provider);
    formModel = unmasked(entry.model);
    maskedPrefill = isMasked(entry.provider) || isMasked(entry.model);
  }

  function declaration() {
    const entry = { provider: formProvider.trim() };
    const model = formModel.trim();
    if (model) entry.model = model;
    if (formOperations.length > 0) entry.operations = [...formOperations];
    for (const feature of FEATURES) {
      if (formFeatures[feature] !== '') entry[feature] = formFeatures[feature] === 'true';
    }
    if (formSchemaDialect) entry.tool_schema_dialect = formSchemaDialect;
    if (formCallDialect) entry.tool_call_dialect = formCallDialect;
    const contextWindow = Number.parseInt(String(formContextWindow), 10);
    if (Number.isFinite(contextWindow) && contextWindow > 0) entry.context_window = contextWindow;
    return entry;
  }

  function planDeclaration(kind) {
    if (!formProvider.trim()) return;
    flow.preview({ kind, ...declaration() });
  }

  function planRemove() {
    const provider = formProvider.trim();
    if (!provider) return;
    const model = formModel.trim();
    flow.preview({ kind: 'remove_model', provider, model: model || undefined });
  }

  function fetchedAt(unix) {
    return unix ? new Date(unix * 1000).toLocaleString() : '—';
  }

  // Load once on mount. The filters apply on Refresh only: a load per
  // keystroke would put a half-typed provider into request URLs.
  $effect(() => {
    untrack(loadModels);
  });
</script>

<section class="space-y-6">
  <div class="flex items-center justify-between">
    <h2 class="text-2xl font-semibold">{t('models.title')}</h2>
  </div>

  <div class="flex flex-wrap items-end gap-2 rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800">
    <div class="min-w-48 flex-1">
      <label for="models-filter-provider" class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('models.provider')}</label>
      <input
        id="models-filter-provider"
        type="text"
        bind:value={filterProvider}
        class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
      />
    </div>
    <div>
      <label for="models-filter-operation" class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('models.operation')}</label>
      <select
        id="models-filter-operation"
        bind:value={filterOperation}
        class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
      >
        <option value="">{t('models.anyOperation')}</option>
        {#each OPERATIONS as operation}
          <option value={operation}>{operation}</option>
        {/each}
      </select>
    </div>
    <label class="flex items-center gap-2 text-sm">
      <input type="checkbox" bind:checked={filterExecutable} />
      {t('models.executableOnly')}
    </label>
    <button
      type="button"
      onclick={loadModels}
      class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
    >
      {t('common.refresh')}
    </button>
  </div>

  {#if settingsError}<ErrorNotice error={settingsError} />{/if}
  {#if settingsCatalog}
    <section class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800" aria-label={t('models.settings')}>
      <h3 class="mb-4 text-lg font-semibold">{t('models.settings')} <span class="font-mono text-sm">{settingsCatalog.model}</span></h3>
      <div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
        <label class="flex flex-col gap-1 text-sm">
          {t('models.level')}
          <select bind:value={settingsLevel} class="rounded-lg border border-gray-300 bg-transparent px-3 py-2 dark:border-gray-600">
            <option value="">{t('models.defaultValue')} ({settingsCatalog.default_level ?? 'none'})</option>
            {#each Object.keys(settingsCatalog.levels ?? {}) as level}<option value={level}>{level}</option>{/each}
          </select>
        </label>
        {#each Object.entries(settingsCatalog.parameters ?? {}) as [name, declaration]}
          {#if declaration.kind !== 'unsupported'}
            <label class="flex flex-col gap-1 text-sm">
              {name.replaceAll('_', ' ')}
              {#if declaration.kind === 'range'}
                <input type="number" min={declaration.min} max={declaration.max} step={declaration.step ?? (['max_output_tokens', 'thinking_budget', 'seed'].includes(name) ? 1 : 'any')}
                  placeholder={t('models.useLevelValue')} bind:value={parameterValues[name]}
                  class="rounded-lg border border-gray-300 bg-transparent px-3 py-2 dark:border-gray-600" />
              {:else}
                <select bind:value={parameterValues[name]} class="rounded-lg border border-gray-300 bg-transparent px-3 py-2 dark:border-gray-600">
                  <option value="">{t('models.useLevelValue')}</option>
                  {#if declaration.kind === 'bool'}
                    <option value={true}>{t('models.enabled')}</option><option value={false}>{t('models.disabled')}</option>
                  {:else}
                    {#each declaration.levels ?? [] as value}<option {value}>{value}</option>{/each}
                  {/if}
                </select>
              {/if}
            </label>
          {/if}
        {/each}
      </div>
      <button type="button" onclick={planSettings} class="mt-4 rounded-lg bg-gray-900 px-4 py-2 text-sm font-medium text-white dark:bg-gray-100 dark:text-gray-900">{t('models.reviewSettings')}</button>
    </section>
  {/if}

  {#if loading}
    <p class="text-sm text-gray-500 dark:text-gray-400">{t('common.loading')}</p>
  {:else if loadError}
    <ErrorNotice error={loadError} />
  {:else if models.length === 0}
    <p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300">
      {t('models.none')}
    </p>
  {:else}
    <div class="overflow-x-auto rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800">
      <table class="w-full text-left text-xs">
        <thead class="text-gray-500 dark:text-gray-400">
          <tr>
            <th class="px-3 py-2">{t('models.provider')}</th>
            <th class="px-3 py-2">{t('models.model')}</th>
            <th class="px-3 py-2">{t('models.operations')}</th>
            <th class="px-3 py-2">{t('models.features')}</th>
            <th class="px-3 py-2">{t('models.contextWindow')}</th>
            <th class="px-3 py-2">{t('models.source')}</th>
            <th class="px-3 py-2">{t('models.fetchedAt')}</th>
            <th class="px-3 py-2"></th>
          </tr>
        </thead>
        <tbody>
          {#each models as entry}
            <tr class="border-t border-gray-200 align-top dark:border-gray-700">
              <td class="px-3 py-2 font-mono">{entry.provider}</td>
              <td class="px-3 py-2 font-mono break-all">{entry.model}</td>
              <td class="px-3 py-2">
                {#each Object.entries(entry.operations ?? {}) as [operation, support]}
                  <div><span class="font-mono">{operation}</span>: {support}</div>
                {/each}
              </td>
              <td class="px-3 py-2">
                {#each Object.entries(entry.features ?? {}) as [feature, support]}
                  <div><span class="font-mono">{feature}</span>: {support}</div>
                {/each}
              </td>
              <td class="px-3 py-2">{entry.context_window ?? '—'}</td>
              <td class="px-3 py-2">{entry.source}</td>
              <td class="px-3 py-2">{fetchedAt(entry.fetched_at_unix)}</td>
              <td class="px-3 py-2">
                <button
                  type="button"
                  onclick={() => edit(entry)}
                  class="rounded-lg border border-gray-300 bg-white px-2 py-1 text-xs text-gray-600 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700"
                >
                  {t('models.declare')}
                </button>
                <button type="button" onclick={() => editSettings(entry)}
                  class="ml-2 rounded-lg border border-gray-300 px-2 py-1 text-xs dark:border-gray-600">
                  {t('models.settings')}
                </button>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {/if}

  <div class="space-y-3 rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800">
    <h3 class="text-base font-semibold">{t('models.declarationTitle')}</h3>
    <p class="text-xs text-gray-500 dark:text-gray-400">{t('models.declarationHint')}</p>
    {#if maskedPrefill}
      <p class="rounded-lg border border-yellow-500/40 bg-yellow-500/10 px-3 py-2 text-sm">{t('manage.maskedPrefill')}</p>
    {/if}
    <div class="grid gap-3 sm:grid-cols-2">
      <div>
        <label for="models-form-provider" class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('models.provider')}</label>
        <input
          id="models-form-provider"
          type="text"
          bind:value={formProvider}
          class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
        />
      </div>
      <div>
        <label for="models-form-model" class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('models.model')}</label>
        <input
          id="models-form-model"
          type="text"
          bind:value={formModel}
          placeholder={t('models.everyModel')}
          class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
        />
      </div>
      <fieldset class="sm:col-span-2">
        <legend class="mb-1 text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('models.operations')}</legend>
        <div class="flex flex-wrap gap-3 text-sm">
          {#each OPERATIONS as operation}
            <label class="flex items-center gap-1">
              <input type="checkbox" value={operation} bind:group={formOperations} />
              <span class="font-mono text-xs">{operation}</span>
            </label>
          {/each}
        </div>
      </fieldset>
      {#each FEATURES as feature}
        <div>
          <label for={`models-form-${feature}`} class="mb-1 block font-mono text-xs text-gray-500 dark:text-gray-400">{feature}</label>
          <select
            id={`models-form-${feature}`}
            bind:value={formFeatures[feature]}
            class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
          >
            <option value="">{t('models.unset')}</option>
            <option value="true">{t('common.yes')}</option>
            <option value="false">{t('common.no')}</option>
          </select>
        </div>
      {/each}
      <div>
        <label for="models-form-schema" class="mb-1 block font-mono text-xs text-gray-500 dark:text-gray-400">tool_schema_dialect</label>
        <select
          id="models-form-schema"
          bind:value={formSchemaDialect}
          class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
        >
          <option value="">{t('models.unset')}</option>
          {#each SCHEMA_DIALECTS as dialect}
            <option value={dialect}>{dialect}</option>
          {/each}
        </select>
      </div>
      <div>
        <label for="models-form-call" class="mb-1 block font-mono text-xs text-gray-500 dark:text-gray-400">tool_call_dialect</label>
        <select
          id="models-form-call"
          bind:value={formCallDialect}
          class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
        >
          <option value="">{t('models.unset')}</option>
          {#each CALL_DIALECTS as dialect}
            <option value={dialect}>{dialect}</option>
          {/each}
        </select>
      </div>
      <div>
        <label for="models-form-context" class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('models.contextWindow')}</label>
        <input
          id="models-form-context"
          type="number"
          min="1"
          bind:value={formContextWindow}
          class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
        />
      </div>
    </div>
    <div class="flex flex-wrap justify-end gap-2">
      <button
        type="button"
        onclick={() => planDeclaration('add_model')}
        disabled={flow.busy || !formProvider.trim()}
        class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"
      >
        {t('models.planAdd')}
      </button>
      <button
        type="button"
        onclick={() => planDeclaration('update_model')}
        disabled={flow.busy || !formProvider.trim()}
        class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 disabled:opacity-50 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
      >
        {t('models.planUpdate')}
      </button>
      <button
        type="button"
        onclick={planRemove}
        disabled={flow.busy || !formProvider.trim()}
        class="rounded-lg border border-red-500/50 bg-red-500/10 px-3 py-2 text-sm text-red-600 hover:bg-red-500/20 disabled:opacity-50 dark:text-red-300"
      >
        {t('models.planRemove')}
      </button>
    </div>
  </div>

  <PlanReview {flow} />
</section>
