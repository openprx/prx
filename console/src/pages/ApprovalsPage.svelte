<script>
  import { ApiError, api } from '../lib/api';
  import { t } from '../lib/i18n';
  import { navigate } from '../lib/router';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import StateBlock from '../lib/ui/StateBlock.svelte';

  let items = $state([]);
  let cursor = $state(null);
  let loading = $state(true);
  let loadingMore = $state(false);
  let failure = $state('');
  let variant = $state('loading');

  function operationText(item) {
    const tool = typeof item?.tool === 'string' && item.tool.length > 0 ? item.tool : t('common.unknown');
    const effect = typeof item?.effect === 'string' && item.effect.length > 0 ? item.effect : t('common.unknown');
    const redacted = item?.arguments_redacted === true ? ` · ${t('approvals.redacted')}` : '';
    return `${tool} · ${effect}${redacted}`;
  }

  async function load({ append = false } = {}) {
    if (append) {
      loadingMore = true;
    } else {
      loading = true;
      failure = '';
    }
    try {
      const response = await api.listApprovals({ limit: 50, cursor: append ? cursor : undefined });
      if (!response || !Array.isArray(response.approvals)) {
        throw new Error(t('approvals.loadFailed'));
      }
      const next = response.approvals.filter((item) => item && typeof item.session_id === 'string');
      items = append ? [...items, ...next] : next;
      cursor = typeof response.next_cursor === 'string' && response.next_cursor.length > 0 ? response.next_cursor : null;
      failure = '';
      variant = '';
    } catch (error) {
      if (!append) {
        items = [];
        cursor = null;
      }
      if (error instanceof ApiError && error.status === 403) {
        variant = 'denied';
        failure = t('approvals.denied');
      } else if (error instanceof ApiError && error.status === 409) {
        variant = 'unavailable';
        failure = t('approvals.unavailable');
      } else if (isAmbiguous(error)) {
        variant = 'disconnected-stale';
        failure = t('common.connectionLost');
      } else {
        variant = 'error';
        failure = t('approvals.loadFailed');
      }
    } finally {
      loading = false;
      loadingMore = false;
    }
  }

  function isAmbiguous(error) {
    return !(error instanceof ApiError);
  }

  function openSession(sessionId) {
    navigate(`/chat/${encodeURIComponent(sessionId)}`);
  }

  $effect(() => {
    load();
  });
</script>

<section class="min-w-0 max-w-full space-y-4">
  <PageHeader title={t('approvals.title')} description={t('approvals.description')} />
  {#if loading}
    <StateBlock variant="loading" message={t('common.loading')} />
  {:else if failure}
    <StateBlock {variant} message={failure} />
  {:else if items.length === 0 && !cursor}
    <StateBlock variant="empty" message={t('approvals.empty')} />
  {:else}
    <ul class="min-w-0 space-y-3">
      {#each items as item (item.decision_id)}
        <li class="min-w-0 space-y-2 rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800">
          <p class="break-all font-mono text-xs">{t('approvals.decision')}: {item.decision_id}</p>
          <p class="break-all text-sm">{t('approvals.session')}: {item.session_id}</p>
          <p class="text-sm">{t('approvals.expires')}: {item.pending_expires_at || t('common.unknown')}</p>
          <p class="break-all text-sm">{t('approvals.operation')}: {operationText(item)}</p>
          <button
            type="button"
            class="rounded-lg bg-sky-600 px-3 py-2 text-sm text-white hover:bg-sky-500"
            onclick={() => openSession(item.session_id)}
          >
            {t('approvals.openSession')}
          </button>
        </li>
      {/each}
    </ul>
    {#if cursor}
      <button
        type="button"
        class="rounded-lg border border-gray-300 px-3 py-2 text-sm dark:border-gray-600"
        disabled={loadingMore}
        onclick={() => load({ append: true })}
      >
        {loadingMore ? t('common.loading') : t('approvals.loadMore')}
      </button>
    {/if}
  {/if}
</section>
