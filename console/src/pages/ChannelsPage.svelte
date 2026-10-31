<script>
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import StateBlock from '../lib/ui/StateBlock.svelte';

  let channels = $state([]);
  let loading = $state(true);
  let errorMessage = $state('');
  let lastUpdated = $state('');

  function humanizeKey(value) {
    if (typeof value !== 'string' || value.length === 0) {
      return t('common.unknown');
    }

    return value
      .replaceAll('_', ' ')
      .split(' ')
      .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
      .join(' ');
  }

  function channelLabel(name) {
    const key = `channels.names.${name}`;
    const translated = t(key);
    return translated === key ? humanizeKey(name) : translated;
  }

  async function loadChannels() {
    try {
      const response = await api.getChannelsStatus();
      channels = Array.isArray(response?.channels) ? response.channels : [];
      errorMessage = '';
      lastUpdated = new Date().toLocaleTimeString();
    } catch (error) {
      errorMessage = error instanceof Error ? error.message : t('channels.loadFailed');
    } finally {
      loading = false;
    }
  }

  $effect(() => {
    let cancelled = false;

    const refresh = async () => {
      if (!cancelled) {
        await loadChannels();
      }
    };

    refresh();
    const timer = setInterval(refresh, 30_000);

    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  });
</script>

<section class="space-y-6">
  <div class="flex items-center justify-between">
    <PageHeader title={t('channels.title')} />
    {#if lastUpdated}
      <span class="text-xs text-gray-500 dark:text-gray-400">{t('common.updatedAt', { time: lastUpdated })}</span>
    {/if}
  </div>

  {#if loading}
    <StateBlock variant="loading" message={t('channels.loading')} />
  {:else if errorMessage}
    <StateBlock variant="error" message={errorMessage} />
  {:else if channels.length === 0}
    <StateBlock variant="empty" message={t('channels.noChannels')} />
  {:else}
    <div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
      {#each channels as channel}
        <article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800">
          <div class="flex items-start justify-between gap-3">
            <h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100">{channelLabel(channel.name)}</h3>
            <span
              class={`rounded-full px-2 py-1 text-xs font-medium ${
                channel.enabled
                  ? 'border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300'
                  : 'border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300'
              }`}
            >
              {channel.enabled ? t('common.enabled') : t('common.disabled')}
            </span>
          </div>
          <p class="mt-3 text-sm text-gray-500 dark:text-gray-400">{t('channels.type')}: {channelLabel(channel.type)}</p>
          <p class="mt-1 text-sm text-gray-500 dark:text-gray-400">{t('channels.status')}: {channelLabel(channel.status)}</p>
        </article>
      {/each}
    </div>
  {/if}
</section>
