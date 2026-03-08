<script>
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';
  import { navigate } from '../lib/router';

  const PAGE_SIZE = 20;
  const STATUS_OPTIONS = ['all', 'active', 'pending', 'empty'];

  let sessions = $state([]);
  let loading = $state(true);
  let refreshing = $state(false);
  let errorMessage = $state('');
  let lastUpdated = $state('');
  let channelFilter = $state('');
  let statusFilter = $state('all');
  let searchQuery = $state('');
  let page = $state(0);
  let hasMore = $state(false);

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

  function statusLabel(name) {
    const key = `sessions.status.${name}`;
    const translated = t(key);
    return translated === key ? humanizeKey(name) : translated;
  }

  const availableChannels = $derived(
    [...new Set(sessions.map((session) => session.channel).filter(Boolean))].sort((left, right) =>
      left.localeCompare(right)
    )
  );

  async function loadSessions({ reset = false, targetPage } = {}) {
    const nextPage = typeof targetPage === 'number' ? targetPage : reset ? 0 : page;
    if (reset) {
      loading = true;
    } else {
      refreshing = true;
    }

    try {
      const response = await api.getSessions({
        limit: PAGE_SIZE + 1,
        offset: nextPage * PAGE_SIZE,
        channel: channelFilter || undefined,
        status: statusFilter === 'all' ? undefined : statusFilter,
        search: searchQuery.trim() || undefined
      });
      const nextSessions = Array.isArray(response) ? response : [];
      hasMore = nextSessions.length > PAGE_SIZE;
      sessions = hasMore ? nextSessions.slice(0, PAGE_SIZE) : nextSessions;
      errorMessage = '';
      lastUpdated = new Date().toLocaleTimeString();
      page = nextPage;
    } catch (error) {
      errorMessage = error instanceof Error ? error.message : t('sessions.loadFailed');
      if (reset) {
        sessions = [];
      }
    } finally {
      loading = false;
      refreshing = false;
    }
  }

  function openSession(sessionId) {
    navigate(`/chat/${encodeURIComponent(sessionId)}`);
  }

  function applyFilters() {
    loadSessions({ reset: true });
  }

  function goToPreviousPage() {
    if (page === 0) {
      return;
    }

    loadSessions({ targetPage: page - 1 });
  }

  function goToNextPage() {
    if (!hasMore) {
      return;
    }

    loadSessions({ targetPage: page + 1 });
  }

  $effect(() => {
    let cancelled = false;

    const refresh = async () => {
      if (cancelled) {
        return;
      }
      await loadSessions({ reset: true });
    };

    refresh();
    const timer = setInterval(refresh, 15_000);

    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  });
</script>

<section class="space-y-6">
  <div class="flex flex-wrap items-center justify-between gap-3">
    <div class="flex items-center gap-3">
      <h2 class="text-2xl font-semibold">{t('sessions.title')}</h2>
      {#if refreshing && !loading}
        <span class="text-xs text-gray-500 dark:text-gray-400">{t('common.loading')}</span>
      {/if}
    </div>
    {#if lastUpdated}
      <span class="text-xs text-gray-500 dark:text-gray-400">{t('common.updatedAt', { time: lastUpdated })}</span>
    {/if}
  </div>

  <div class="grid gap-3 rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800 lg:grid-cols-[minmax(0,1.3fr)_220px_220px_auto]">
    <input
      type="search"
      bind:value={searchQuery}
      placeholder={t('sessions.searchPlaceholder')}
      class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
      onkeydown={(event) => {
        if (event.key === 'Enter') {
          applyFilters();
        }
      }}
    />
    <select
      bind:value={channelFilter}
      class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
    >
      <option value="">{t('sessions.allChannels')}</option>
      {#each availableChannels as channel}
        <option value={channel}>{channelLabel(channel)}</option>
      {/each}
    </select>
    <select
      bind:value={statusFilter}
      class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
    >
      {#each STATUS_OPTIONS as option}
        <option value={option}>{statusLabel(option)}</option>
      {/each}
    </select>
    <button
      type="button"
      onclick={applyFilters}
      class="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white transition hover:bg-sky-500"
    >
      {t('sessions.applyFilters')}
    </button>
  </div>

  {#if loading}
    <p class="text-sm text-gray-500 dark:text-gray-400">{t('sessions.loading')}</p>
  {:else if errorMessage}
    <p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300">
      {errorMessage}
    </p>
  {:else if sessions.length === 0}
    <p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300">
      {t('sessions.none')}
    </p>
  {:else}
    <div class="overflow-x-auto rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800">
      <table class="min-w-full divide-y divide-gray-200 text-sm dark:divide-gray-700">
        <thead class="bg-gray-50 text-left text-gray-600 dark:bg-gray-900/50 dark:text-gray-300">
          <tr>
            <th class="px-4 py-3 font-semibold">{t('sessions.sessionId')}</th>
            <th class="px-4 py-3 font-semibold">{t('sessions.sender')}</th>
            <th class="px-4 py-3 font-semibold">{t('sessions.channel')}</th>
            <th class="px-4 py-3 font-semibold">{t('sessions.statusLabel')}</th>
            <th class="px-4 py-3 font-semibold">{t('sessions.messages')}</th>
            <th class="px-4 py-3 font-semibold">{t('sessions.lastMessage')}</th>
          </tr>
        </thead>
        <tbody class="divide-y divide-gray-200 text-gray-700 dark:divide-gray-700 dark:text-gray-200">
          {#each sessions as session}
            <tr
              class="cursor-pointer transition hover:bg-gray-50 dark:hover:bg-gray-700/40"
              onclick={() => openSession(session.session_id)}
            >
              <td class="px-4 py-3 font-mono text-xs">{session.session_id}</td>
              <td class="px-4 py-3">{session.sender}</td>
              <td class="px-4 py-3">{channelLabel(session.channel)}</td>
              <td class="px-4 py-3">
                <span class="rounded-full border border-gray-300/70 px-2 py-1 text-xs dark:border-gray-600/70">
                  {statusLabel(session.status)}
                </span>
              </td>
              <td class="px-4 py-3">{session.message_count}</td>
              <td class="px-4 py-3">{session.last_message_preview || t('common.empty')}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>

    <div class="flex items-center justify-between gap-3">
      <p class="text-sm text-gray-500 dark:text-gray-400">
        {t('sessions.pageLabel', { page: page + 1 })}
      </p>
      <div class="flex items-center gap-2">
        <button
          type="button"
          onclick={goToPreviousPage}
          disabled={page === 0}
          class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 disabled:cursor-not-allowed disabled:opacity-50 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
        >
          {t('sessions.previousPage')}
        </button>
        <button
          type="button"
          onclick={goToNextPage}
          disabled={!hasMore}
          class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 disabled:cursor-not-allowed disabled:opacity-50 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
        >
          {t('sessions.nextPage')}
        </button>
      </div>
    </div>
  {/if}
</section>
