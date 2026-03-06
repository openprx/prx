<script>
  import { api } from '../lib/api';
  import { formatUptime } from '../lib/format';
  import { t } from '../lib/i18n';

  let status = $state(null);
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

  const cards = $derived([
    { label: t('overview.version'), value: status?.version ?? t('common.na') },
    {
      label: t('overview.uptime'),
      value:
        typeof status?.uptime_seconds === 'number'
          ? formatUptime(status.uptime_seconds)
          : t('common.na')
    },
    { label: t('overview.model'), value: status?.model ?? t('common.na') },
    { label: t('overview.memoryBackend'), value: status?.memory_backend ?? t('common.na') },
    {
      label: t('overview.gatewayPort'),
      value: status?.gateway_port ? String(status.gateway_port) : t('common.na')
    }
  ]);

  const channels = $derived(Array.isArray(status?.channels) ? status.channels : []);

  async function loadStatus() {
    try {
      const response = await api.getStatus();
      status = response;
      errorMessage = '';
      lastUpdated = new Date().toLocaleTimeString();
    } catch (error) {
      errorMessage = error instanceof Error ? error.message : t('overview.loadFailed');
    } finally {
      loading = false;
    }
  }

  $effect(() => {
    let cancelled = false;

    const refresh = async () => {
      if (!cancelled) {
        await loadStatus();
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
    <h2 class="text-2xl font-semibold">{t('overview.title')}</h2>
    {#if lastUpdated}
      <span class="text-xs text-gray-400">{t('common.updatedAt', { time: lastUpdated })}</span>
    {/if}
  </div>

  {#if loading}
    <p class="text-sm text-gray-400">{t('overview.loading')}</p>
  {:else if errorMessage}
    <p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-300">
      {errorMessage}
    </p>
  {:else}
    <div class="grid gap-4 sm:grid-cols-2 xl:grid-cols-5">
      {#each cards as card}
        <div class="rounded-xl border border-gray-700 bg-gray-800 p-4">
          <p class="text-xs uppercase tracking-wide text-gray-400">{card.label}</p>
          <p class="mt-2 text-lg font-semibold text-gray-100">{card.value}</p>
        </div>
      {/each}
    </div>

    <div class="rounded-xl border border-gray-700 bg-gray-800 p-4">
      <h3 class="text-sm font-semibold uppercase tracking-wide text-gray-300">
        {t('overview.configuredChannels')}
      </h3>
      {#if channels.length === 0}
        <p class="mt-3 text-sm text-gray-400">{t('overview.noChannelsConfigured')}</p>
      {:else}
        <ul class="mt-3 flex flex-wrap gap-2">
          {#each channels as channel}
            <li class="rounded-full border border-gray-600 bg-gray-900 px-3 py-1 text-sm text-gray-200">
              {channelLabel(channel)}
            </li>
          {/each}
        </ul>
      {/if}
    </div>
  {/if}
</section>
