<script>
  import { onDestroy, untrack } from 'svelte';
  import { api } from '../lib/api';
  import { ChangeFlow, isMasked, unmasked } from '../lib/change-flow.svelte.js';
  import PlanReview from '../lib/PlanReview.svelte';
  import { t } from '../lib/i18n';
  import { loadFailureVariant } from '../lib/load-state';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import StateBlock from '../lib/ui/StateBlock.svelte';

  const SELECTION_OPERATIONS = ['chat', 'embedding'];

  let defaults = $state(null);
  let routes = $state([]);
  let agents = $state([]);
  let loadError = $state(null);

  let defaultOperation = $state('chat');
  let defaultProvider = $state('');
  let defaultModel = $state('');
  // Switching the chat default away from a provider that owns the root key
  // needs an explicit choice: keep the key for the new provider, or clear it.
  let rootCredential = $state('');

  let routeOperation = $state('chat');
  let routeHint = $state('');
  let routeProvider = $state('');
  let routeModel = $state('');
  // The route as loaded when "Edit" filled the form, with the operation
  // tab it came from: an update of that same route sends only the fields
  // the operator changed.
  let loadedRoute = $state(null);
  // A masked provider or model was left out of the route or the agent form.
  let routeMaskedPrefill = $state(false);
  let agentMaskedPrefill = $state(false);

  let agentName = $state('');
  let agentProvider = $state('');
  let agentModel = $state('');

  let alive = true;
  let routeRequest = 0;
  const flow = new ChangeFlow(afterApply);
  onDestroy(() => { alive = false; flow.dispose(); });

  // After an apply (or a stale plan) the forms no longer describe what is
  // stored: forget the loaded route and the masked-value warnings.
  async function afterApply() {
    loadedRoute = null;
    routeMaskedPrefill = false;
    agentMaskedPrefill = false;
    await loadAll();
  }

  async function loadAll() {
    if (!alive) return;
    const request = ++routeRequest;
    const operation = routeOperation;
    try {
      const [defaultsResponse, routesResponse, agentsResponse] = await Promise.all([
        api.getDefaults(),
        api.getRoutes(operation),
        api.getAgents()
      ]);
      if (!alive || request !== routeRequest || operation !== routeOperation) return;
      defaults = defaultsResponse;
      routes = Array.isArray(routesResponse?.routes) ? routesResponse.routes : [];
      agents = Array.isArray(agentsResponse?.agents) ? agentsResponse.agents : [];
      loadError = null;
    } catch (e) {
      if (!alive || request !== routeRequest || operation !== routeOperation) return;
      loadError = e;
    }
  }

  async function showRoutes(operation) {
    routeOperation = operation;
    routes = [];
    loadError = null;
    loadedRoute = null;
    routeMaskedPrefill = false;
    await loadAll();
  }

  function planDefault() {
    const change = {
      kind: 'set_default',
      operation: defaultOperation,
      provider: defaultProvider.trim(),
      model: defaultModel.trim()
    };
    if (defaultOperation === 'chat' && rootCredential) change.root_credential = rootCredential;
    flow.preview(change);
  }

  function editRoute(route) {
    loadedRoute = { ...route, operation: routeOperation };
    routeHint = route.hint;
    routeProvider = unmasked(route.provider);
    routeModel = unmasked(route.model);
    routeMaskedPrefill = isMasked(route.provider) || isMasked(route.model);
  }

  function planRoute(kind) {
    const hint = routeHint.trim();
    const change = { kind, operation: routeOperation, hint };
    const provider = routeProvider.trim();
    const model = routeModel.trim();
    // An update leaves out what it does not change.
    const loaded =
      kind === 'update_route' && loadedRoute?.hint === hint && loadedRoute?.operation === routeOperation
        ? loadedRoute
        : null;
    if (provider && provider !== loaded?.provider) change.provider = provider;
    if (model && model !== loaded?.model) change.model = model;
    flow.preview(change);
  }

  function planRemoveRoute(hint) {
    flow.preview({ kind: 'remove_route', operation: routeOperation, hint });
  }

  function editAgent(agent) {
    agentName = agent.name;
    agentProvider = unmasked(agent.provider);
    agentModel = unmasked(agent.model);
    agentMaskedPrefill = isMasked(agent.provider) || isMasked(agent.model);
  }

  function planAgent() {
    flow.preview({
      kind: 'bind_agent',
      agent: agentName.trim(),
      provider: agentProvider.trim(),
      model: agentModel.trim()
    });
  }

  // Load once on mount; later loads follow an explicit action only.
  $effect(() => {
    untrack(loadAll);
  });
</script>

<section class="space-y-6">
  <div class="flex items-center justify-between">
    <PageHeader title={t('routing.title')} />
    <button
      type="button"
      onclick={loadAll}
      class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
    >
      {t('common.refresh')}
    </button>
  </div>

  {#if loadError}
    <StateBlock variant={loadFailureVariant(loadError)} message={loadError?.message || ''} />
  {:else if !defaults}
    <StateBlock variant="loading" message={t('common.loading')} />
  {/if}
  <PlanReview {flow} />
  {#if defaults}

  <div class="space-y-3 rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800">
    <h3 class="text-base font-semibold">{t('routing.defaults')}</h3>
    {#if defaults}
      <dl class="grid gap-1 text-sm sm:grid-cols-2">
        <dt class="text-gray-500 dark:text-gray-400">{t('routing.chatDefault')}</dt>
        <dd class="font-mono">{defaults.chat?.provider} / {defaults.chat?.model}</dd>
        <dt class="text-gray-500 dark:text-gray-400">{t('routing.status')}</dt>
        <dd>
          {defaults.chat?.resolved
            ? defaults.chat?.verified
              ? t('routing.verified')
              : t('routing.unverified')
            : t('routing.unresolved')}
          <span class="block text-xs text-gray-500 dark:text-gray-400">{defaults.chat?.status}</span>
        </dd>
        <dt class="text-gray-500 dark:text-gray-400">{t('routing.embeddingDefault')}</dt>
        <dd class="font-mono">{defaults.embedding?.provider} / {defaults.embedding?.model}</dd>
      </dl>
    {/if}
    <div class="grid gap-3 sm:grid-cols-4">
      <div>
        <label for="routing-default-operation" class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('routing.operation')}</label>
        <select
          id="routing-default-operation"
          bind:value={defaultOperation}
          class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
        >
          {#each SELECTION_OPERATIONS as operation}
            <option value={operation}>{operation}</option>
          {/each}
        </select>
      </div>
      <div>
        <label for="routing-default-provider" class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('routing.provider')}</label>
        <input
          id="routing-default-provider"
          type="text"
          bind:value={defaultProvider}
          class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
        />
      </div>
      <div>
        <label for="routing-default-model" class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('routing.model')}</label>
        <input
          id="routing-default-model"
          type="text"
          bind:value={defaultModel}
          class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
        />
      </div>
      {#if defaultOperation === 'chat'}
        <div>
          <label for="routing-root-credential" class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('routing.rootCredential')}</label>
          <select
            id="routing-root-credential"
            bind:value={rootCredential}
            class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
          >
            <option value="">{t('routing.rootCredentialAsk')}</option>
            <option value="keep">{t('routing.rootCredentialKeep')}</option>
            <option value="clear">{t('routing.rootCredentialClear')}</option>
          </select>
        </div>
      {/if}
    </div>
    <div class="flex justify-end">
      <button
        type="button"
        onclick={planDefault}
        disabled={flow.busy || !defaultProvider.trim() || !defaultModel.trim()}
        class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"
      >
        {t('manage.planChange')}
      </button>
    </div>
  </div>

  <div class="space-y-3 rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800">
    <div class="flex flex-wrap items-center justify-between gap-2">
      <h3 class="text-base font-semibold">{t('routing.routes')}</h3>
      <div class="flex gap-1">
        {#each SELECTION_OPERATIONS as operation}
          <button
            type="button"
            onclick={() => showRoutes(operation)}
            class={`rounded-lg px-3 py-1 text-xs ${
              routeOperation === operation
                ? 'bg-sky-600 text-white'
                : 'border border-gray-300 text-gray-700 dark:border-gray-600 dark:text-gray-200'
            }`}
          >
            {operation}
          </button>
        {/each}
      </div>
    </div>
    {#if routeMaskedPrefill}
      <p class="rounded-lg border border-yellow-500/40 bg-yellow-500/10 px-3 py-2 text-sm">{t('manage.maskedPrefill')}</p>
    {/if}
    {#if routes.length === 0}
      <p class="text-sm text-gray-500 dark:text-gray-400">{t('routing.noRoutes')}</p>
    {:else}
      <table class="w-full text-left text-xs">
        <thead class="text-gray-500 dark:text-gray-400">
          <tr>
            <th class="py-1 pr-3">{t('routing.hint')}</th>
            <th class="py-1 pr-3">{t('routing.provider')}</th>
            <th class="py-1 pr-3">{t('routing.model')}</th>
            <th class="py-1 pr-3">{t('routing.ownKey')}</th>
            <th class="py-1"></th>
          </tr>
        </thead>
        <tbody>
          {#each routes as route (route.hint)}
            <tr class="border-t border-gray-200 dark:border-gray-700">
              <td class="py-1 pr-3 font-mono">{route.hint}</td>
              <td class="py-1 pr-3 font-mono">{route.provider}</td>
              <td class="py-1 pr-3 font-mono break-all">{route.model}</td>
              <td class="py-1 pr-3">{route.has_own_key ? t('common.yes') : t('common.no')}</td>
              <td class="flex justify-end gap-2 py-1">
                <button
                  type="button"
                  onclick={() => editRoute(route)}
                  class="rounded-lg border border-gray-300 bg-white px-2 py-1 text-xs text-gray-600 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700"
                >
                  {t('routing.edit')}
                </button>
                <button
                  type="button"
                  onclick={() => planRemoveRoute(route.hint)}
                  disabled={flow.busy}
                  class="rounded-lg border border-red-500/50 bg-red-500/10 px-2 py-1 text-xs text-red-600 hover:bg-red-500/20 disabled:opacity-50 dark:text-red-300"
                >
                  {t('routing.remove')}
                </button>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/if}
    <div class="grid gap-3 sm:grid-cols-3">
      <div>
        <label for="routing-route-hint" class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('routing.hint')}</label>
        <input
          id="routing-route-hint"
          type="text"
          bind:value={routeHint}
          class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
        />
      </div>
      <div>
        <label for="routing-route-provider" class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('routing.provider')}</label>
        <input
          id="routing-route-provider"
          type="text"
          bind:value={routeProvider}
          class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
        />
      </div>
      <div>
        <label for="routing-route-model" class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('routing.model')}</label>
        <input
          id="routing-route-model"
          type="text"
          bind:value={routeModel}
          class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
        />
      </div>
    </div>
    <div class="flex justify-end gap-2">
      <button
        type="button"
        onclick={() => planRoute('add_route')}
        disabled={flow.busy || !routeHint.trim() || !routeProvider.trim() || !routeModel.trim()}
        class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"
      >
        {t('routing.planAdd')}
      </button>
      <button
        type="button"
        onclick={() => planRoute('update_route')}
        disabled={flow.busy || !routeHint.trim()}
        class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 disabled:opacity-50 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
      >
        {t('routing.planUpdate')}
      </button>
    </div>
  </div>

  <div class="space-y-3 rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800">
    <h3 class="text-base font-semibold">{t('routing.agents')}</h3>
    {#if agentMaskedPrefill}
      <p class="rounded-lg border border-yellow-500/40 bg-yellow-500/10 px-3 py-2 text-sm">{t('manage.maskedPrefill')}</p>
    {/if}
    {#if agents.length === 0}
      <p class="text-sm text-gray-500 dark:text-gray-400">{t('routing.noAgents')}</p>
    {:else}
      <ul class="space-y-1 text-sm">
        {#each agents as agent (agent.name)}
          <li class="flex items-center justify-between gap-2">
            <span><span class="font-mono">{agent.name}</span>: <span class="font-mono">{agent.provider} / {agent.model}</span></span>
            <button
              type="button"
              onclick={() => editAgent(agent)}
              class="rounded-lg border border-gray-300 bg-white px-2 py-1 text-xs text-gray-600 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700"
            >
              {t('routing.edit')}
            </button>
          </li>
        {/each}
      </ul>
    {/if}
    <div class="grid gap-3 sm:grid-cols-3">
      <div>
        <label for="routing-agent-name" class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('routing.agent')}</label>
        <input
          id="routing-agent-name"
          type="text"
          bind:value={agentName}
          class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
        />
      </div>
      <div>
        <label for="routing-agent-provider" class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('routing.provider')}</label>
        <input
          id="routing-agent-provider"
          type="text"
          bind:value={agentProvider}
          class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
        />
      </div>
      <div>
        <label for="routing-agent-model" class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('routing.model')}</label>
        <input
          id="routing-agent-model"
          type="text"
          bind:value={agentModel}
          class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
        />
      </div>
    </div>
    <div class="flex justify-end">
      <button
        type="button"
        onclick={planAgent}
        disabled={flow.busy || !agentName.trim() || !agentProvider.trim() || !agentModel.trim()}
        class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"
      >
        {t('manage.planChange')}
      </button>
    </div>
  </div>
  {/if}
</section>
