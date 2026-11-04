<script>
  import { api } from '../lib/api';
  import { presentContext } from '../lib/context-status';
  import { formatUptime } from '../lib/format';
  import { t } from '../lib/i18n';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import StateBlock from '../lib/ui/StateBlock.svelte';

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
  const health = $derived(presentContext(status, '').filter((fact) => fact.id !== 'origin' && fact.id !== 'version'));

  function namedList(value) {
    if (!Array.isArray(value)) {
      return t('common.unknown');
    }
    const names = value.filter((item) => typeof item === 'string' && item.trim().length > 0).map((item) => item.trim());
    return names.length > 0 ? names.join(', ') : t('operations.overview.none');
  }

  function promptCacheSummary(rows) {
    const totals = (Array.isArray(rows) ? rows : []).reduce((sum, row) => ({
      hits: sum.hits + (row.cache_hit_requests_total ?? 0),
      reported: sum.reported + (row.cache_reported_total ?? 0),
      cont: sum.cont + (row.series_continuations_total ?? 0),
      sys: sum.sys + (row.system_prefix_changed_total ?? 0),
      tools: sum.tools + (row.tools_prefix_changed_total ?? 0)
    }), { hits: 0, reported: 0, cont: 0, sys: 0, tools: 0 });
    if (totals.reported > 0) return t('operations.overview.promptCacheSummary', totals);
    const unreported = t('operations.overview.promptCacheNotReported');
    return totals.cont > 0
      ? `${unreported} · ${t('operations.overview.promptCachePrefixes', totals)}`
      : unreported;
  }

  function circuitList(rows) {
    const active = Array.isArray(rows) ? rows.filter((row) => row.state !== 'closed') : [];
    return active.length > 0
      ? active.map((row) => `${row.provider} (${t(row.state === 'half_open' ? 'operations.overview.circuitHalfOpen' : 'operations.overview.circuitOpen')}, ${Math.ceil((row.retry_in_ms ?? 0) / 1000)}s)`).join(', ')
      : t('operations.overview.none');
  }

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
    <PageHeader title={t('overview.title')} />
    {#if lastUpdated}
      <span class="text-xs text-gray-500 dark:text-gray-400">{t('common.updatedAt', { time: lastUpdated })}</span>
    {/if}
  </div>

  {#if loading}
    <StateBlock variant="loading" message={t('overview.loading')} />
  {:else if errorMessage}
    <StateBlock variant="error" message={errorMessage} />
  {:else}
    <div class="grid gap-4 sm:grid-cols-2 xl:grid-cols-5">
      {#each cards as card}
        <div class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800">
          <p class="text-xs uppercase tracking-wide text-gray-500 dark:text-gray-400">{card.label}</p>
          <p class="mt-2 text-lg font-semibold text-gray-900 dark:text-gray-100">{card.value}</p>
        </div>
      {/each}
    </div>

    <div class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800">
      <h3 class="text-sm font-semibold uppercase tracking-wide text-gray-600 dark:text-gray-300">
        {t('operations.overview.health')}
      </h3>
      <dl class="mt-3 grid gap-3 sm:grid-cols-2">
        {#each health as fact (fact.id)}
          <div>
            <dt class="text-xs uppercase tracking-wide text-gray-500 dark:text-gray-400">{fact.label}</dt>
            <dd class="mt-1 text-sm text-gray-900 dark:text-gray-100">{fact.value}</dd>
          </div>
        {/each}
        <div>
          <dt class="text-xs uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('operations.overview.promptCache')}</dt>
          <dd class="mt-1 text-sm text-gray-900 dark:text-gray-100">{promptCacheSummary(status?.prompt_cache)}</dd>
        </div>
        <div>
          <dt class="text-xs uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('operations.overview.participants')}</dt>
          <dd class="mt-1 text-sm text-gray-900 dark:text-gray-100">{namedList(status?.config_generation_participants)}</dd>
        </div>
        <div>
          <dt class="text-xs uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('operations.overview.unavailableProviders')}</dt>
          <dd class="mt-1 text-sm text-gray-900 dark:text-gray-100">{namedList(status?.provider_unavailable)}</dd>
        </div>
        <div>
          <dt class="text-xs uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('operations.overview.providerCircuits')}</dt>
          <dd class="mt-1 text-sm text-gray-900 dark:text-gray-100">{circuitList(status?.provider_circuits)}</dd>
        </div>
      </dl>
    </div>

    <div class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800">
      <h3 class="text-sm font-semibold uppercase tracking-wide text-gray-600 dark:text-gray-300">
        {t('overview.configuredChannels')}
      </h3>
      {#if channels.length === 0}
        <p class="mt-3 text-sm text-gray-500 dark:text-gray-400">{t('overview.noChannelsConfigured')}</p>
      {:else}
        <ul class="mt-3 flex flex-wrap gap-2">
          {#each channels as channel}
            <li class="rounded-full border border-gray-300 bg-gray-50 px-3 py-1 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200">
              {channelLabel(channel)}
            </li>
          {/each}
        </ul>
      {/if}
    </div>
  {/if}
</section>
