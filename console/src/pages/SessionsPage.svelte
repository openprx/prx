<script>
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';
  import { navigate } from '../lib/router';

  let sessions = $state([]);
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

  async function loadSessions() {
    try {
      const response = await api.getSessions();
      sessions = Array.isArray(response) ? response : [];
      errorMessage = '';
      lastUpdated = new Date().toLocaleTimeString();
    } catch (error) {
      errorMessage = error instanceof Error ? error.message : t('sessions.loadFailed');
    } finally {
      loading = false;
    }
  }

  function openSession(sessionId) {
    navigate(`/chat/${encodeURIComponent(sessionId)}`);
  }

  $effect(() => {
    let cancelled = false;

    const refresh = async () => {
      if (!cancelled) {
        await loadSessions();
      }
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
  <div class="flex items-center justify-between">
    <h2 class="text-2xl font-semibold">{t('sessions.title')}</h2>
    {#if lastUpdated}
      <span class="text-xs text-gray-500 dark:text-gray-400">{t('common.updatedAt', { time: lastUpdated })}</span>
    {/if}
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
              <td class="px-4 py-3">{session.message_count}</td>
              <td class="px-4 py-3">{session.last_message_preview || t('common.empty')}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {/if}
</section>
