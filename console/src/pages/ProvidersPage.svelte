<script>
  import { onDestroy, untrack } from 'svelte';
  import { api } from '../lib/api';
  import { ChangeFlow, isMasked, sendConfirming, unmasked } from '../lib/change-flow.svelte.js';
  import ErrorNotice from '../lib/ErrorNotice.svelte';
  import PlanReview from '../lib/PlanReview.svelte';
  import { t } from '../lib/i18n';

  const REFERENCE_PREFIX = 'cred-';

  let providers = $state([]);
  let revision = $state(null);
  let loading = $state(true);
  let loadError = $state(null);

  let newProvider = $state('');
  let selected = $state('');
  let details = $state(null);
  let detailsError = $state(null);
  let endpointValue = $state('');

  // Credentials are write-only: each value lives only in its field until it
  // is sent once in a request body, and the field is cleared whatever the
  // answer. Nothing is kept in storage, a URL or the page state.
  let credentialName = $state('');
  let credentialValue = $state('');
  let rootKeyValue = $state('');
  let credentialBusy = $state(false);
  let credentialError = $state(null);
  let credentialNotice = $state('');

  let testModel = $state('');
  let testBusy = $state(false);
  let testReport = $state(null);
  let testError = $state(null);

  let alive = true;
  let providerRequest = 0;
  let detailRequest = 0;
  const flow = new ChangeFlow(reloadAll);
  onDestroy(() => {
    alive = false;
    flow.dispose();
    credentialValue = '';
    rootKeyValue = '';
  });

  async function loadProviders() {
    if (!alive) return;
    const request = ++providerRequest;
    loading = true;
    try {
      const response = await api.getProviders();
      if (!alive || request !== providerRequest) return;
      providers = Array.isArray(response?.providers) ? response.providers : [];
      revision = response?.revision ?? null;
      loadError = null;
    } catch (e) {
      if (!alive || request !== providerRequest) return;
      providers = [];
      loadError = e;
    } finally {
      if (alive && request === providerRequest) loading = false;
    }
  }

  async function loadDetails() {
    if (!alive) return;
    const request = ++detailRequest;
    const provider = selected;
    if (!selected) {
      details = null;
      return;
    }
    try {
      const response = await api.getProvider(provider);
      if (!alive || request !== detailRequest || selected !== provider) return;
      details = response;
      // A masked endpoint is shown, never prefilled: changing it means
      // typing the full URL again.
      endpointValue = unmasked(details?.provider?.endpoint);
      detailsError = null;
    } catch (e) {
      if (!alive || request !== detailRequest || selected !== provider) return;
      details = null;
      detailsError = e;
    }
  }

  async function reloadAll() {
    if (!alive) return;
    await loadProviders();
    if (alive) await loadDetails();
  }

  async function select(provider) {
    if (!alive || credentialBusy || testBusy) return;
    // A value typed for one provider never carries over to another: the
    // reference field would file it under the new provider, and the root
    // key field would keep it hidden until the default is shown again.
    credentialValue = '';
    rootKeyValue = '';
    credentialName = '';
    // A masked provider (a custom URL with userinfo or a query value) cannot
    // be addressed from the Console; the gateway refuses it in a URL.
    if (isMasked(provider)) return;
    selected = provider;
    details = null;
    detailsError = null;
    endpointValue = '';
    testReport = null;
    testError = null;
    credentialError = null;
    credentialNotice = '';
    await loadDetails();
  }

  function planAdd() {
    const provider = newProvider.trim();
    if (!provider) return;
    flow.preview({ kind: 'add_provider', provider });
  }

  function planRemove(provider) {
    flow.preview({ kind: 'remove_provider', provider });
  }

  function planEndpoint() {
    const url = endpointValue.trim();
    if (!url) return;
    flow.preview({ kind: 'update_provider', provider: selected, api_url: url });
  }

  function planClearEndpoint() {
    flow.preview({ kind: 'update_provider', provider: selected, api_url: null });
  }

  async function submitCredential() {
    if (!alive || credentialBusy) return;
    const value = credentialValue;
    if (!selected || !value.trim()) return;
    credentialBusy = true;
    credentialError = null;
    credentialNotice = '';
    try {
      await api.setProviderCredential(selected, value, credentialName.trim());
      if (!alive) return;
      credentialNotice = t('providers.referenceStored');
      await loadDetails();
    } catch (e) {
      if (!alive) return;
      credentialError = e;
    } finally {
      if (alive) { credentialValue = ''; credentialBusy = false; }
    }
  }

  async function removeReference(reference) {
    if (!alive || credentialBusy) return;
    const name = reference.id.startsWith(REFERENCE_PREFIX) ? reference.id.slice(REFERENCE_PREFIX.length) : reference.id;
    if (!window.confirm(t('providers.removeReferenceConfirm', { id: reference.id }))) return;
    credentialBusy = true;
    credentialError = null;
    credentialNotice = '';
    try {
      await api.removeProviderCredential(selected, name);
      if (!alive) return;
      credentialNotice = t('providers.referenceRemoved');
      await loadDetails();
    } catch (e) {
      if (!alive) return;
      credentialError = e;
    } finally {
      if (alive) credentialBusy = false;
    }
  }

  // The root connection key (`api_key`) is what PRX sends the default
  // provider today. The slot endpoint confirms a plain rotation itself; any
  // other high-risk difference answers 428 and is confirmed like every
  // other change.
  async function submitRootKey() {
    if (!alive || credentialBusy) return;
    const value = rootKeyValue;
    if (!value.trim()) return;
    credentialBusy = true;
    credentialError = null;
    credentialNotice = '';
    try {
      await sendConfirming((options) => api.setConfigSecret('api_key', value, options), revision, () => alive);
      if (!alive) return;
      credentialNotice = t('providers.rootKeyStored');
      await reloadAll();
    } catch (e) {
      if (!alive) return;
      credentialError = e;
    } finally {
      if (alive) { rootKeyValue = ''; credentialBusy = false; }
    }
  }

  async function runTest(live) {
    if (!alive || testBusy) return;
    if (live && !window.confirm(t('providers.liveConfirm'))) return;
    testBusy = true;
    testError = null;
    testReport = null;
    try {
      const response = await api.testProvider(selected, { model: testModel.trim(), live });
      if (alive) testReport = response;
    } catch (e) {
      if (!alive) return;
      testError = e;
    } finally {
      if (alive) testBusy = false;
    }
  }

  // Load once on mount; later loads follow an explicit action only.
  $effect(() => {
    untrack(loadProviders);
  });
</script>

<section class="space-y-6">
  <div class="flex items-center justify-between">
    <h2 class="text-2xl font-semibold">{t('providers.title')}</h2>
    <button
      type="button"
      onclick={reloadAll}
      class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
    >
      {t('common.refresh')}
    </button>
  </div>

  <div class="flex flex-wrap items-end gap-2 rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800">
    <div class="min-w-64 flex-1">
      <label for="provider-add" class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">
        {t('providers.addLabel')}
      </label>
      <input
        id="provider-add"
        type="text"
        bind:value={newProvider}
        placeholder={t('providers.addPlaceholder')}
        class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
      />
    </div>
    <button
      type="button"
      onclick={planAdd}
      disabled={flow.busy || !newProvider.trim()}
      class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"
    >
      {t('manage.planChange')}
    </button>
  </div>

  <PlanReview {flow} />

  {#if loading}
    <p class="text-sm text-gray-500 dark:text-gray-400">{t('common.loading')}</p>
  {:else if loadError}
    <ErrorNotice error={loadError} />
  {:else if providers.length === 0}
    <p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300">
      {t('providers.none')}
    </p>
  {:else}
    <div class="space-y-3">
      {#each providers as entry (entry.identity)}
        <article
          class={`rounded-xl border bg-white p-4 dark:bg-gray-800 ${
            selected === entry.provider ? 'border-sky-500' : 'border-gray-200 dark:border-gray-700'
          }`}
        >
          <div class="flex flex-wrap items-start justify-between gap-3">
            <div class="min-w-0 flex-1 space-y-1">
              <div class="flex flex-wrap items-center gap-2">
                <h3 class="font-mono text-base font-semibold">{entry.provider}</h3>
                {#if entry.default}
                  <span class="rounded-full border border-sky-500/50 bg-sky-500/20 px-2 py-0.5 text-xs">{t('providers.default')}</span>
                {/if}
                {#if entry.fallback}
                  <span class="rounded-full border border-gray-400/50 px-2 py-0.5 text-xs">{t('providers.fallback')}</span>
                {/if}
              </div>
              <p class="font-mono text-xs text-gray-500 dark:text-gray-400">{entry.identity}</p>
              {#if entry.endpoint}
                <p class="text-xs text-gray-500 dark:text-gray-400">{t('providers.endpoint')}: <span class="font-mono">{entry.endpoint}</span></p>
              {/if}
              <p class="text-xs text-gray-500 dark:text-gray-400">
                {t('providers.declarations', { count: entry.declarations })}
              </p>
              {#if entry.used_by?.length > 0}
                <p class="text-xs text-gray-500 dark:text-gray-400">{t('manage.usedBy')} {entry.used_by.join(', ')}</p>
              {/if}
              {#if isMasked(entry.provider)}
                <p class="text-xs text-yellow-600 dark:text-yellow-300">{t('providers.maskedProvider')}</p>
              {/if}
            </div>
            <div class="flex items-center gap-2">
              <button
                type="button"
                onclick={() => select(entry.provider)}
                disabled={credentialBusy || testBusy || isMasked(entry.provider)}
                class="rounded-lg border border-gray-300 bg-white px-2 py-1 text-xs text-gray-600 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700"
              >
                {t('providers.manage')}
              </button>
              <button
                type="button"
                onclick={() => planRemove(entry.provider)}
                disabled={flow.busy || isMasked(entry.provider)}
                class="rounded-lg border border-red-500/50 bg-red-500/10 px-2 py-1 text-xs text-red-600 hover:bg-red-500/20 disabled:opacity-50 dark:text-red-300"
              >
                {t('providers.remove')}
              </button>
            </div>
          </div>
        </article>
      {/each}
    </div>
  {/if}

  {#if selected}
    <div class="space-y-4 rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800">
      <h3 class="font-mono text-lg font-semibold">{selected}</h3>
      <ErrorNotice error={detailsError} />

      {#if details}
        {@const credentials = details.credentials}
        {#if details.provider?.default}
          <div class="flex flex-wrap items-end gap-2">
            <div class="min-w-64 flex-1">
              <label for="provider-endpoint" class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">
                {t('providers.endpoint')}
              </label>
              <input
                id="provider-endpoint"
                type="url"
                bind:value={endpointValue}
                placeholder={t('providers.endpointPlaceholder')}
                class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
              />
              {#if isMasked(details.provider?.endpoint)}
                <p class="mt-1 text-xs text-gray-500 dark:text-gray-400">
                  {t('providers.endpointMasked', { endpoint: details.provider.endpoint })}
                </p>
              {/if}
            </div>
            <button
              type="button"
              onclick={planEndpoint}
              disabled={flow.busy || !endpointValue.trim()}
              class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 disabled:opacity-50 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
            >
              {t('manage.planChange')}
            </button>
            {#if details.provider?.endpoint}
              <button
                type="button"
                onclick={planClearEndpoint}
                disabled={flow.busy}
                class="rounded-lg border border-red-500/50 bg-red-500/10 px-3 py-2 text-sm text-red-600 hover:bg-red-500/20 disabled:opacity-50 dark:text-red-300"
              >
                {t('providers.clearEndpoint')}
              </button>
            {/if}
          </div>
        {/if}

        <div class="space-y-2">
          <h4 class="text-sm font-semibold">{t('providers.credentials')}</h4>
          <p class="text-sm text-gray-600 dark:text-gray-300">{credentials?.guidance}</p>
          <dl class="grid gap-1 text-xs text-gray-500 dark:text-gray-400 sm:grid-cols-2">
            <dt>{t('providers.inUse')}</dt>
            <dd class="font-mono">{credentials?.in_use}</dd>
            <dt>{t('providers.rootConnection')}</dt>
            <dd>{credentials?.root_connection ? t('common.yes') : t('common.no')}</dd>
            <dt>{t('providers.inlineSlots')}</dt>
            <dd class="font-mono">{credentials?.inline_slots?.join(', ') || '—'}</dd>
            <dt>{t('providers.storeProfiles')}</dt>
            <dd class="font-mono">{credentials?.store_profiles?.join(', ') || '—'}</dd>
          </dl>

          {#if credentials?.references?.length > 0}
            <ul class="space-y-1 text-xs">
              {#each credentials.references as reference (reference.id)}
                <li class="flex items-center justify-between gap-2">
                  <span class="font-mono">{reference.id}</span>
                  <span class="text-gray-500 dark:text-gray-400">{t('providers.updatedAt', { time: reference.updated_at })}</span>
                  <button
                    type="button"
                    onclick={() => removeReference(reference)}
                    disabled={credentialBusy}
                    class="rounded-lg border border-red-500/50 bg-red-500/10 px-2 py-1 text-xs text-red-600 hover:bg-red-500/20 disabled:opacity-50 dark:text-red-300"
                  >
                    {t('providers.remove')}
                  </button>
                </li>
              {/each}
            </ul>
          {/if}
        </div>

        {#if details.provider?.default}
          <div class="flex flex-wrap items-end gap-2">
            <div class="min-w-64 flex-1">
              <label for="provider-root-key" class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">
                {t('providers.rootKey')}
              </label>
              <input
                id="provider-root-key"
                type="password"
                autocomplete="new-password"
                bind:value={rootKeyValue}
                class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
              />
            </div>
            <button
              type="button"
              onclick={submitRootKey}
              disabled={credentialBusy || !rootKeyValue.trim()}
              class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"
            >
              {t('providers.setKey')}
            </button>
          </div>
        {/if}

        <div class="flex flex-wrap items-end gap-2">
          <div class="w-48">
            <label for="provider-credential-name" class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">
              {t('providers.referenceName')}
            </label>
            <input
              id="provider-credential-name"
              type="text"
              bind:value={credentialName}
              placeholder={t('providers.referenceNamePlaceholder')}
              class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
            />
          </div>
          <div class="min-w-64 flex-1">
            <label for="provider-credential-value" class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">
              {t('providers.referenceValue')}
            </label>
            <input
              id="provider-credential-value"
              type="password"
              autocomplete="new-password"
              bind:value={credentialValue}
              class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
            />
          </div>
          <button
            type="button"
            onclick={submitCredential}
            disabled={credentialBusy || !credentialValue.trim()}
            class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"
          >
            {t('providers.storeReference')}
          </button>
        </div>
        <p class="text-xs text-gray-500 dark:text-gray-400">{t('providers.referenceHint')}</p>

        <ErrorNotice error={credentialError} />
        {#if credentialNotice}
          <p class="rounded-lg border border-green-500/40 bg-green-500/10 px-3 py-2 text-sm">{credentialNotice}</p>
        {/if}

        <div class="space-y-2">
          <h4 class="text-sm font-semibold">{t('providers.test')}</h4>
          <div class="flex flex-wrap items-end gap-2">
            <div class="min-w-64 flex-1">
              <label for="provider-test-model" class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">
                {t('providers.testModel')}
              </label>
              <input
                id="provider-test-model"
                type="text"
                bind:value={testModel}
                class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
              />
            </div>
            <button
              type="button"
              onclick={() => runTest(false)}
              disabled={testBusy}
              class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 disabled:opacity-50 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
            >
              {t('providers.testLocal')}
            </button>
            <button
              type="button"
              onclick={() => runTest(true)}
              disabled={testBusy}
              class="rounded-lg border border-yellow-500/50 bg-yellow-500/10 px-3 py-2 text-sm hover:bg-yellow-500/20 disabled:opacity-50"
            >
              {t('providers.testLive')}
            </button>
          </div>
          <ErrorNotice error={testError} />
          {#if testReport}
            <dl class="grid gap-1 text-xs sm:grid-cols-2">
              <dt>{t('providers.resolved')}</dt>
              <dd>{testReport.local?.resolved ? t('common.yes') : t('common.no')}</dd>
              <dt>{t('providers.verified')}</dt>
              <dd>{testReport.local?.verified ? t('common.yes') : t('common.no')}</dd>
              <dt>{t('providers.detail')}</dt>
              <dd>{testReport.local?.detail}</dd>
              {#if testReport.live}
                <dt>{t('providers.reachable')}</dt>
                <dd>{testReport.live.reachable ? t('common.yes') : t('common.no')}</dd>
                <dt>{t('providers.httpStatus')}</dt>
                <dd>{testReport.live.http_status_class ?? '—'}</dd>
                <dt>{t('providers.modelListed')}</dt>
                <dd>{testReport.live.model_listed === null || testReport.live.model_listed === undefined ? '—' : testReport.live.model_listed ? t('common.yes') : t('common.no')}</dd>
                <dt>{t('providers.latency')}</dt>
                <dd>{testReport.live.latency_ms ?? '—'}</dd>
                {#if testReport.live.error}
                  <dt>{t('common.error')}</dt>
                  <dd>{testReport.live.error}</dd>
                {/if}
              {/if}
            </dl>
          {/if}
        </div>
      {/if}
    </div>
  {/if}
</section>
