<script>
  import { ApiError, api } from '../lib/api';
  import { t } from '../lib/i18n';
  import ConfirmDialog from '../lib/ui/ConfirmDialog.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import StateBlock from '../lib/ui/StateBlock.svelte';

  let loading = $state(true);
  let failure = $state('');
  let variant = $state('loading');
  let sessions = $state([]);
  let selectedId = $state('');
  let task = $state('');
  let disposition = $state('queue');
  let confirmOpen = $state(false);
  let notice = $state('');
  let uncertain = $state(false);
  let results = $state([]);
  let missed = $state(false);
  let afterSeq = 0;
  let ticket = 0;

  function publicSession(row) {
    if (!row || typeof row !== 'object') {
      return null;
    }
    const copy = { ...row };
    delete copy.token;
    delete copy.session_token;
    return copy;
  }

  async function loadSessions() {
    loading = true;
    failure = '';
    const current = ++ticket;
    try {
      const response = await api.listChatSessions();
      if (current !== ticket) {
        return;
      }
      const rows = Array.isArray(response?.sessions) ? response.sessions.map(publicSession).filter(Boolean) : null;
      if (!rows) {
        throw new Error(t('terminals.loadFailed'));
      }
      sessions = rows;
      variant = '';
      if (selectedId && !sessions.some((session) => session.session_id === selectedId)) {
        selectedId = '';
      }
    } catch (error) {
      if (current !== ticket) {
        return;
      }
      sessions = [];
      if (error instanceof ApiError && error.status === 403) {
        variant = 'denied';
        failure = t('terminals.denied');
      } else if (!(error instanceof ApiError)) {
        variant = 'disconnected-stale';
        failure = t('chat.pollStale');
      } else {
        variant = 'error';
        failure = error.message || t('terminals.loadFailed');
      }
    } finally {
      if (current === ticket) {
        loading = false;
      }
    }
  }

  async function loadResults() {
    const requested = afterSeq;
    try {
      const response = await api.listChatResults({ afterSeq: requested, limit: 50 });
      const rows = Array.isArray(response?.results) ? response.results : [];
      if (typeof response?.oldest_seq === 'number' && requested < response.oldest_seq) {
        missed = true;
      }
      results = [...results, ...rows];
      if (typeof response?.next_seq === 'number') {
        afterSeq = response.next_seq;
      }
    } catch (error) {
      if (error instanceof ApiError) {
        notice = error.message;
      } else {
        uncertain = true;
      }
    }
  }

  function askAssign() {
    if (!selectedId || task.trim().length === 0) {
      return;
    }
    uncertain = false;
    notice = '';
    confirmOpen = true;
  }

  async function confirmAssign() {
    const sessionId = selectedId;
    const text = task.trim();
    const mode = disposition;
    confirmOpen = false;
    const current = ++ticket;
    try {
      const response = await api.assignChatSession(sessionId, { task: text, disposition: mode });
      if (current !== ticket || selectedId !== sessionId) {
        return;
      }
      if (response?.status === 429) {
        notice = t('terminals.rateLimited');
        return;
      }
      const body = response?.body || {};
      notice = `${t('terminals.assigned')} ${body.assignment_id || ''} ${body.work_id || ''} ${body.disposition || mode}`.trim();
      task = '';
      await loadSessions();
      await loadResults();
    } catch (error) {
      if (current !== ticket) {
        return;
      }
      if (error instanceof ApiError && error.status === 429) {
        notice = t('terminals.rateLimited');
        return;
      }
      if (error instanceof ApiError) {
        notice = error.message || t('terminals.assignFailed');
        return;
      }
      uncertain = true;
    }
  }

  const confirmConsequence = $derived(
    disposition === 'steer' ? t('terminals.confirmSteer') : disposition === 'interrupt' ? t('terminals.confirmInterrupt') : t('terminals.confirmQueue')
  );

  $effect(() => {
    loadSessions();
    loadResults();
  });
</script>

<section class="min-w-0 max-w-full space-y-4">
  <PageHeader title={t('terminals.title')} description={t('terminals.description')} />
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('terminals.noToken')}</p>
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('activity.processWide')}</p>
  {#if loading}
    <StateBlock variant="loading" message={t('common.loading')} />
  {:else if failure}
    <StateBlock {variant} message={failure} />
  {:else if sessions.length === 0}
    <StateBlock variant="empty" message={t('terminals.empty')} />
  {:else}
    <div class="overflow-x-auto rounded-xl border border-gray-200 dark:border-gray-700">
      <table class="min-w-full text-sm">
        <thead>
          <tr>
            <th class="px-3 py-2 text-left">{t('terminals.session')}</th>
            <th class="px-3 py-2 text-left">{t('terminals.label')}</th>
            <th class="px-3 py-2 text-left">{t('terminals.liveness')}</th>
            <th class="px-3 py-2 text-left">{t('terminals.pid')}</th>
            <th class="px-3 py-2 text-left">{t('terminals.queued')}</th>
          </tr>
        </thead>
        <tbody>
          {#each sessions as session (session.session_id)}
            <tr>
              <td class="break-all px-3 py-2 font-mono text-xs">{session.session_id}</td>
              <td class="px-3 py-2">{session.label || t('common.unknown')}</td>
              <td class="px-3 py-2">{session.liveness || t('common.unknown')}</td>
              <td class="px-3 py-2">{session.pid ?? t('common.unknown')}</td>
              <td class="px-3 py-2">{session.queued ?? 0}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
    <label class="block text-sm">
      <span class="mb-1 block">{t('terminals.select')}</span>
      <select class="w-full max-w-xl rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm dark:border-gray-600 dark:bg-gray-900" bind:value={selectedId}>
        <option value="">{t('terminals.select')}</option>
        {#each sessions as session (session.session_id)}
          <option value={session.session_id}>{session.label || session.session_id}</option>
        {/each}
      </select>
    </label>
    <label class="block text-sm">
      <span class="mb-1 block">{t('terminals.task')}</span>
      <textarea bind:value={task} class="min-h-20 w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm dark:border-gray-600 dark:bg-gray-900"></textarea>
    </label>
    <label class="block text-sm">
      <span class="mb-1 block">{t('terminals.disposition')}</span>
      <select bind:value={disposition} class="w-full max-w-xs rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm dark:border-gray-600 dark:bg-gray-900">
        <option value="queue">{t('terminals.queue')}</option>
        <option value="steer">{t('terminals.steer')}</option>
        <option value="interrupt">{t('terminals.interrupt')}</option>
      </select>
    </label>
    <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm text-white" onclick={askAssign}>
      {t('terminals.assign')}
    </button>
  {/if}

  {#if notice}
    <p role="status" class="break-all text-sm">{notice}</p>
  {/if}
  {#if uncertain}
    <p role="status" class="text-sm">{t('terminals.uncertain')}</p>
  {/if}

  <h3 class="text-lg font-semibold">{t('terminals.results')}</h3>
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('terminals.summaryNote')}</p>
  {#if missed}
    <StateBlock variant="disconnected-stale" message={t('terminals.missed')} />
  {/if}
  {#if results.length === 0}
    <StateBlock variant="empty" message={t('terminals.emptyResults')} />
  {:else}
    <ul class="min-w-0 space-y-2">
      {#each results as result (result.seq)}
        <li class="break-all rounded-lg border border-gray-200 p-3 text-sm dark:border-gray-700">
          {t('terminals.seq')} {result.seq} · {result.session_label || result.session_id} · {result.disposition} · {result.status}
          <p>{t('terminals.summary')}: {result.summary || t('common.empty')}</p>
        </li>
      {/each}
    </ul>
  {/if}
</section>

<ConfirmDialog
  open={confirmOpen}
  title={t('terminals.confirmTitle')}
  objectName={selectedId}
  consequence={confirmConsequence}
  onCancel={() => { confirmOpen = false; }}
  onConfirm={confirmAssign}
/>
