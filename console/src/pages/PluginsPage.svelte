<script>
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';
  import { Blocks, RefreshCw, CheckCircle, AlertCircle, Loader } from '@lucide/svelte';

  let plugins = $state([]);
  let loading = $state(true);
  let errorMessage = $state('');
  let reloadingName = $state('');
  let toast = $state('');
  let toastType = $state('success');

  function showToast(msg, type = 'success') {
    toast = msg;
    toastType = type;
    setTimeout(() => { toast = ''; }, 3000);
  }

  async function loadPlugins() {
    loading = true;
    try {
      const response = await api.getPlugins();
      plugins = Array.isArray(response?.plugins) ? response.plugins : [];
      errorMessage = '';
    } catch {
      plugins = [];
      errorMessage = t('plugins.loadFailed');
    } finally {
      loading = false;
    }
  }

  async function reloadPlugin(name) {
    reloadingName = name;
    try {
      await api.reloadPlugin(name);
      showToast(t('plugins.reloadSuccess', { name }));
      await loadPlugins();
    } catch (e) {
      showToast(t('plugins.reloadFailed') + (e.message ? `: ${e.message}` : ''), 'error');
    } finally {
      reloadingName = '';
    }
  }

  function statusColor(status) {
    if (typeof status === 'string' && status === 'Active') return 'text-green-500';
    if (typeof status === 'object' && status?.Error) return 'text-red-500';
    return 'text-yellow-500';
  }

  function statusLabel(status) {
    if (typeof status === 'string' && status === 'Active') return t('plugins.statusActive');
    if (typeof status === 'object' && status?.Error) return status.Error;
    return t('common.unknown');
  }

  $effect(() => {
    loadPlugins();
  });
</script>

{#if toast}
  <div
    class={`fixed right-4 top-4 z-50 rounded-lg px-4 py-2 text-sm font-medium text-white shadow-lg transition ${
      toastType === 'error' ? 'bg-red-600' : 'bg-green-600'
    }`}
  >
    {toast}
  </div>
{/if}

<section class="space-y-6">
  <div class="flex items-center justify-between">
    <div class="flex items-center gap-2">
      <Blocks size={24} />
      <h2 class="text-2xl font-semibold">{t('plugins.title')}</h2>
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
    <p class="text-sm text-gray-500 dark:text-gray-400">{t('plugins.loading')}</p>
  {:else if errorMessage}
    <div class="rounded-lg border border-red-200 bg-red-50 p-4 text-sm text-red-600 dark:border-red-800 dark:bg-red-900/20 dark:text-red-400">
      {errorMessage}
    </div>
  {:else if plugins.length === 0}
    <div class="rounded-lg border border-gray-200 bg-gray-50 p-8 text-center dark:border-gray-700 dark:bg-gray-800">
      <Blocks size={40} class="mx-auto mb-3 text-gray-400 dark:text-gray-500" />
      <p class="text-sm text-gray-500 dark:text-gray-400">{t('plugins.noPlugins')}</p>
    </div>
  {:else}
    <div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
      {#each plugins as plugin}
        <div class="rounded-lg border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800">
          <div class="mb-3 flex items-start justify-between">
            <div>
              <h3 class="font-semibold text-gray-900 dark:text-gray-100">{plugin.name}</h3>
              <p class="text-xs text-gray-500 dark:text-gray-400">v{plugin.version}</p>
            </div>
            <div class="flex items-center gap-1 {statusColor(plugin.status)}">
              {#if typeof plugin.status === 'string' && plugin.status === 'Active'}
                <CheckCircle size={16} />
              {:else}
                <AlertCircle size={16} />
              {/if}
              <span class="text-xs">{statusLabel(plugin.status)}</span>
            </div>
          </div>

          {#if plugin.description}
            <p class="mb-3 text-sm text-gray-600 dark:text-gray-300">{plugin.description}</p>
          {/if}

          {#if plugin.capabilities?.length}
            <div class="mb-3">
              <p class="mb-1 text-xs font-medium text-gray-500 dark:text-gray-400">{t('plugins.capabilities')}</p>
              <div class="flex flex-wrap gap-1">
                {#each plugin.capabilities as cap}
                  <span class="rounded-full bg-sky-100 px-2 py-0.5 text-xs text-sky-700 dark:bg-sky-900/30 dark:text-sky-300">
                    {cap}
                  </span>
                {/each}
              </div>
            </div>
          {/if}

          {#if plugin.permissions_required?.length}
            <div class="mb-3">
              <p class="mb-1 text-xs font-medium text-gray-500 dark:text-gray-400">{t('plugins.permissions')}</p>
              <div class="flex flex-wrap gap-1">
                {#each plugin.permissions_required as perm}
                  <span class="rounded-full bg-amber-100 px-2 py-0.5 text-xs text-amber-700 dark:bg-amber-900/30 dark:text-amber-300">
                    {perm}
                  </span>
                {/each}
              </div>
            </div>
          {/if}

          <div class="flex justify-end">
            <button
              type="button"
              onclick={() => reloadPlugin(plugin.name)}
              disabled={reloadingName === plugin.name}
              class="flex items-center gap-1 rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-xs text-gray-700 transition hover:bg-gray-100 disabled:opacity-50 dark:border-gray-600 dark:bg-gray-700 dark:text-gray-200 dark:hover:bg-gray-600"
            >
              {#if reloadingName === plugin.name}
                <Loader size={14} class="animate-spin" />
              {:else}
                <RefreshCw size={14} />
              {/if}
              {t('plugins.reload')}
            </button>
          </div>
        </div>
      {/each}
    </div>
  {/if}
</section>
