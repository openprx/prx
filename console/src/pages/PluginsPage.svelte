<script>
  import { Blocks, RefreshCw, CheckCircle, AlertCircle, Loader } from '@lucide/svelte';
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';
  import { loadFailureVariant, pluginActionKind } from '../lib/load-state';
  import ConfirmDialog from '../lib/ui/ConfirmDialog.svelte';
  import Field from '../lib/ui/Field.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import StateBlock from '../lib/ui/StateBlock.svelte';

  let plugins = $state([]);
  let generation = $state(null);
  let featureNote = $state('');
  let loading = $state(true);
  let loadError = $state(null);
  let reloadingName = $state('');
  let acting = $state(false);
  let actionKind = $state('');
  let actionError = $state('');
  let actionResult = $state(null);
  let pending = $state(null);

  let enableName = $state('');
  let source = $state('');
  let sha256 = $state('');
  let manifestSha256 = $state('');
  let permissionsText = $state('');
  let grantNone = $state(false);

  const HEX_64 = /^[0-9a-fA-F]{64}$/;

  function showResult(response) {
    actionResult = response && typeof response === 'object' ? response : {};
    actionKind = '';
    actionError = '';
  }

  function showFailure(error) {
    actionResult = null;
    actionKind = pluginActionKind(error);
    actionError = error instanceof Error ? error.message : t('plugins.denied');
  }

  async function runAction(work) {
    if (acting) return;
    acting = true;
    actionKind = '';
    actionError = '';
    try {
      showResult(await work());
      await loadPlugins();
    } catch (error) {
      showFailure(error);
    } finally {
      acting = false;
    }
  }

  function permissionList() {
    return permissionsText
      .split('\n')
      .map((line) => line.trim())
      .filter((line) => line.length > 0);
  }

  const reviewReady = $derived(
    source.trim().length > 0 &&
      HEX_64.test(sha256.trim()) &&
      HEX_64.test(manifestSha256.trim()) &&
      (grantNone || permissionList().length > 0)
  );

  function reviewedBody() {
    return {
      source: source.trim(),
      sha256: sha256.trim(),
      manifest_sha256: manifestSha256.trim(),
      permissions: grantNone ? [] : permissionList()
    };
  }

  function reviewText(body) {
    const permissions = body.permissions.length === 0 ? t('plugins.grantNone') : body.permissions.join(', ');
    return t('plugins.reviewConsequence', {
      sha256: body.sha256,
      manifest: body.manifest_sha256,
      permissions
    });
  }

  async function loadPlugins() {
    loading = true;
    try {
      const response = await api.getPlugins();
      plugins = Array.isArray(response?.plugins) ? response.plugins : [];
      generation = Object.hasOwn(response ?? {}, 'generation') ? response.generation : null;
      featureNote = typeof response?.note === 'string' ? response.note : '';
      loadError = null;
    } catch (error) {
      plugins = [];
      generation = null;
      featureNote = '';
      loadError = error;
    } finally {
      loading = false;
    }
  }

  async function reloadPlugin(name) {
    reloadingName = name;
    try {
      showResult(await api.reloadPlugin(name));
      await loadPlugins();
    } catch (error) {
      showFailure(error);
    } finally {
      reloadingName = '';
    }
  }

  function statusLabel(status) {
    if (typeof status === 'string') return status;
    if (status && typeof status === 'object' && typeof status.Error === 'string') return status.Error;
    return t('common.unknown');
  }

  function confirmPending() {
    const action = pending;
    pending = null;
    if (!action) return;
    if (action.kind === 'remove') {
      runAction(() => api.removePlugin(action.name));
    } else if (action.kind === 'install') {
      runAction(() => api.installPlugin(action.body));
    } else if (action.kind === 'update') {
      runAction(() => api.updatePlugin(action.body));
    }
  }

  $effect(() => {
    loadPlugins();
  });
</script>

<section class="space-y-6">
  <div class="flex items-center justify-between gap-3">
    <div class="flex items-center gap-2">
      <Blocks size={24} />
      <PageHeader title={t('plugins.title')} />
    </div>
    <button
      type="button"
      onclick={loadPlugins}
      class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
    >
      {t('common.refresh')}
    </button>
  </div>

  {#if loading}
    <StateBlock variant="loading" message={t('plugins.loading')} />
  {:else if loadError}
    <StateBlock variant={loadFailureVariant(loadError)} message={loadError?.message || t('plugins.loadFailed')} />
  {:else}
    <div class="space-y-2 text-sm text-gray-600 dark:text-gray-300">
      <p>{t('plugins.exactArtifact')}</p>
      <p>{t('plugins.notFirstDiscoverable')}</p>
      <p>{t('plugins.contractGap')}</p>
      {#if generation !== null}
        <p class="font-mono text-xs">{t('plugins.generation')}: {String(generation)}</p>
      {/if}
    </div>

    {#if featureNote}
      <StateBlock variant="unavailable" message={featureNote} />
    {:else if plugins.length === 0}
      <StateBlock variant="empty" message={t('plugins.noPlugins')} />
    {:else}
      <div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
        {#each plugins as plugin}
          <article class="rounded-lg border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800">
            <div class="mb-3 flex items-start justify-between gap-3">
              <div>
                <h3 class="font-semibold text-gray-900 dark:text-gray-100">{plugin.name}</h3>
                <p class="text-xs text-gray-500 dark:text-gray-400">v{plugin.version}</p>
                <p class="text-xs text-gray-500 dark:text-gray-400">{t('plugins.loadedFact')}</p>
              </div>
              <div class="text-right text-xs text-gray-600 dark:text-gray-300">
                <p>{t('plugins.registryStatus')}</p>
                <p class="flex items-center justify-end gap-1">
                  {#if plugin.status === 'Active'}
                    <CheckCircle size={14} />
                  {:else}
                    <AlertCircle size={14} />
                  {/if}
                  {statusLabel(plugin.status)}
                </p>
              </div>
            </div>
            {#if plugin.description}
              <p class="mb-3 text-sm text-gray-600 dark:text-gray-300">{plugin.description}</p>
            {/if}
            {#if plugin.capabilities?.length}
              <p class="mb-1 text-xs font-medium text-gray-500 dark:text-gray-400">{t('plugins.capabilities')}</p>
              <p class="mb-3 text-xs">{plugin.capabilities.join(', ')}</p>
            {/if}
            <p class="mb-1 text-xs font-medium text-gray-500 dark:text-gray-400">{t('plugins.required')}</p>
            <p class="mb-3 text-xs">{plugin.permissions_required?.length ? plugin.permissions_required.join(', ') : t('channels.resultMissing')}</p>
            <p class="mb-1 text-xs font-medium text-gray-500 dark:text-gray-400">{t('plugins.granted')}</p>
            <p class="mb-3 text-xs">{plugin.permissions_granted?.length ? plugin.permissions_granted.join(', ') : t('channels.resultMissing')}</p>
            <div class="flex flex-wrap justify-end gap-2">
              <button
                type="button"
                onclick={() => reloadPlugin(plugin.name)}
                disabled={reloadingName === plugin.name || acting}
                aria-label={t('plugins.reloadNamed', { name: plugin.name })}
                class="flex items-center gap-1 rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-xs text-gray-700 disabled:opacity-50 dark:border-gray-600 dark:bg-gray-700 dark:text-gray-200"
              >
                {#if reloadingName === plugin.name}
                  <Loader size={14} class="animate-spin" />
                {:else}
                  <RefreshCw size={14} />
                {/if}
                {t('plugins.reload')}
              </button>
              <button
                type="button"
                onclick={() => runAction(() => api.disablePlugin(plugin.name))}
                disabled={acting}
                aria-label={t('plugins.disableNamed', { name: plugin.name })}
                class="rounded-lg border border-gray-300 px-3 py-1.5 text-xs disabled:opacity-50 dark:border-gray-600"
              >
                {t('plugins.disable')}
              </button>
              <button
                type="button"
                onclick={() => (pending = { kind: 'remove', name: plugin.name })}
                disabled={acting}
                aria-label={t('plugins.removeNamed', { name: plugin.name })}
                class="rounded-lg border border-red-500/50 px-3 py-1.5 text-xs text-red-600 disabled:opacity-50 dark:text-red-300"
              >
                {t('plugins.remove')}
              </button>
            </div>
          </article>
        {/each}
      </div>
    {/if}

    {#if !featureNote}
      <form
        class="space-y-3 rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"
        onsubmit={(event) => {
          event.preventDefault();
          runAction(() => api.enablePlugin(enableName.trim()));
        }}
      >
        <h3 class="text-base font-semibold">{t('plugins.enable')}</h3>
        <Field name="enable-name" label={t('plugins.enableName')} value={enableName} onValue={(value) => (enableName = value)} />
        <button type="submit" disabled={acting || enableName.trim().length === 0} class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white disabled:opacity-50">
          {t('plugins.enable')}
        </button>
      </form>

      <div class="space-y-2">
        <button
          type="button"
          onclick={() => runAction(() => api.refreshPlugins())}
          disabled={acting}
          class="rounded-lg border border-gray-300 px-3 py-2 text-sm disabled:opacity-50 dark:border-gray-600"
        >
          {t('plugins.refreshAll')}
        </button>
        <p class="text-xs text-gray-500 dark:text-gray-400">{t('plugins.refreshNote')}</p>
      </div>

      <form class="space-y-3 rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800" onsubmit={(event) => event.preventDefault()}>
        <h3 class="text-base font-semibold">{t('plugins.install')}</h3>
        <p class="text-xs text-gray-500 dark:text-gray-400">{t('plugins.noBytes')}</p>
        <Field name="plugin-source" label={t('plugins.source')} value={source} onValue={(value) => (source = value)} />
        <Field name="plugin-sha256" label={t('plugins.sha256')} value={sha256} onValue={(value) => (sha256 = value)} />
        <Field name="plugin-manifest-sha256" label={t('plugins.manifestSha256')} value={manifestSha256} onValue={(value) => (manifestSha256 = value)} />
        <div class="ui-field">
          <label for="plugin-permissions">{t('plugins.permissions')}</label>
          <textarea id="plugin-permissions" name="permissions" rows="3" bind:value={permissionsText} disabled={grantNone} class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm disabled:opacity-50 dark:border-gray-600 dark:bg-gray-900"></textarea>
        </div>
        <label class="flex items-center gap-2 text-sm text-gray-700 dark:text-gray-200">
          <input type="checkbox" bind:checked={grantNone} />
          {t('plugins.grantNone')}
        </label>
        <div class="flex flex-wrap gap-2">
          <button
            type="button"
            disabled={acting || !reviewReady}
            onclick={() => (pending = { kind: 'install', name: source.trim(), body: reviewedBody() })}
            class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white disabled:opacity-50"
          >
            {t('plugins.install')}
          </button>
          <button
            type="button"
            disabled={acting || !reviewReady}
            onclick={() => (pending = { kind: 'update', name: source.trim(), body: reviewedBody() })}
            class="rounded-lg border border-gray-300 px-3 py-2 text-sm disabled:opacity-50 dark:border-gray-600"
          >
            {t('plugins.update')}
          </button>
        </div>
      </form>
    {/if}

    {#if actionKind === 'denied'}
      <StateBlock variant="denied" message={actionError || t('plugins.denied')} />
    {:else if actionKind === 'conflict'}
      <StateBlock variant="error" message={actionError || t('plugins.conflict')} />
    {:else if actionKind === 'confirm'}
      <StateBlock variant="error" message={t('plugins.confirmRequired')} />
    {:else if actionKind === 'absent'}
      <StateBlock variant="empty" message={actionError || t('plugins.absent')} />
    {:else if actionKind === 'unavailable'}
      <StateBlock variant="unavailable" message={actionError || t('plugins.featureOff')} />
    {:else if actionKind === 'error'}
      <StateBlock variant="error" message={actionError} />
    {/if}
    {#if actionResult}
      <div class="space-y-1">
        <p class="text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('plugins.actionResult')}</p>
        <pre class="max-h-72 overflow-auto rounded-lg bg-gray-100 p-3 text-xs dark:bg-gray-900">{JSON.stringify(actionResult, null, 2)}</pre>
      </div>
    {/if}
  {/if}
</section>

<ConfirmDialog
  open={pending !== null}
  title={pending?.kind === 'remove' ? t('plugins.removeTitle') : pending?.kind === 'update' ? t('plugins.update') : t('plugins.install')}
  objectName={pending?.kind === 'remove' ? (pending?.name ?? '') : (pending?.body?.source ?? '')}
  consequence={pending?.kind === 'remove' ? t('plugins.removeConsequence') : pending?.body ? reviewText(pending.body) : ''}
  onConfirm={confirmPending}
  onCancel={() => (pending = null)}
/>
