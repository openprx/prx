<script>
  import { onDestroy, untrack } from 'svelte';
  import { api } from '../lib/api';
  import { riskReasonText } from '../lib/change-flow.svelte.js';
  import ErrorNotice from '../lib/ErrorNotice.svelte';
  import { t } from '../lib/i18n';
  import { loadFailureVariant } from '../lib/load-state';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import StateBlock from '../lib/ui/StateBlock.svelte';

  // The confirmations a rollback's 428 can ask for, each a body flag of
  // the retry; `if_match` is answered by the revision the 428 reports.
  const ROLLBACK_CONFIRMATIONS = ['confirm', 'restore_credentials', 'disable_encryption'];

  let records = $state([]);
  let revision = $state(null);
  let loading = $state(true);
  let loadError = $state(null);

  // A rollback under review: what the restore writes (`plan`), the recorded
  // change it undoes (`undoes`), and the confirmation the server asks for.
  let review = $state(null);
  let busy = $state(false);
  let actionError = $state(null);
  let result = $state(null);
  let reloading = $state(false);
  let reloadResponse = $state(null);
  let reloadError = $state(null);
  let reloadStatus = $state(null);
  // Bumped when a newer reload starts or the page is destroyed, so a poll
  // does not write state after the operator has left.
  let reloadGeneration = 0;

  onDestroy(() => {
    reloadGeneration += 1;
  });

  const RELOAD_FIELDS = [
    ['config_reload_status', 'changes.reloadStatus'],
    ['config_reload_in_progress', 'changes.reloadInProgress'],
    ['active_config_generation', 'changes.activeGeneration'],
    ['active_config_source_revision', 'changes.activeRevision'],
    ['desired_config_source_revision', 'changes.desiredRevision'],
    ['restart_required', 'changes.restartRequired'],
    ['config_generation_participants', 'changes.participants'],
    ['last_config_reload_failure', 'changes.lastFailure']
  ];

  async function loadRecords() {
    loading = true;
    try {
      const response = await api.getConfigHistory();
      records = Array.isArray(response?.records) ? response.records : [];
      revision = response?.revision ?? null;
      loadError = null;
    } catch (e) {
      records = [];
      loadError = e;
    } finally {
      loading = false;
    }
  }

  function requiredText(required) {
    if (required === 'restore_credentials') return t('changes.requiredRestoreCredentials');
    if (required === 'disable_encryption') return t('changes.requiredDisableEncryption');
    return t('changes.requiredConfirm');
  }

  // Send the rollback with the confirmations given so far, bound to
  // `boundRevision` (If-Match). A 428 names what else it needs and the
  // revision its restore plan was computed against: show both plans and ask.
  async function sendRollback(record, flags, boundRevision) {
    busy = true;
    actionError = null;
    try {
      result = await api.rollbackConfig(record.id, flags, { revision: boundRevision });
      review = null;
      await loadRecords();
    } catch (e) {
      const required = e?.body?.required;
      const refused = e?.body?.revision;
      if (e?.status === 428 && refused && (ROLLBACK_CONFIRMATIONS.includes(required) || required === 'if_match')) {
        review = {
          record,
          flags,
          required,
          revision: refused,
          refusal: e,
          reasons: Array.isArray(e.body.risk_reasons) ? e.body.risk_reasons : [],
          plan: e.body.plan ?? '',
          undoes: e.body.undoes ?? ''
        };
      } else {
        review = null;
        actionError = e;
        if (e?.status === 409) await loadRecords();
      }
    } finally {
      busy = false;
    }
  }

  function statusField(payload, key) {
    if (!payload || typeof payload !== 'object' || !Object.hasOwn(payload, key)) {
      return t('changes.fieldMissing');
    }
    const value = payload[key];
    if (typeof value === 'boolean') return value ? 'true' : 'false';
    if (value === null || value === undefined) return 'null';
    if (typeof value === 'object') return JSON.stringify(value);
    return String(value);
  }

  async function reloadConfiguration() {
    if (reloading) return;
    const generation = reloadGeneration + 1;
    reloadGeneration = generation;
    reloading = true;
    reloadError = null;
    reloadResponse = null;
    try {
      const response = await api.reloadConfig();
      if (generation !== reloadGeneration) return;
      reloadResponse = response && typeof response === 'object' ? response : {};
      for (let attempt = 0; attempt < 5; attempt += 1) {
        const status = await api.getStatus();
        if (generation !== reloadGeneration) return;
        reloadStatus = status && typeof status === 'object' ? status : {};
        if (!reloadStatus.config_reload_in_progress) break;
        await new Promise((resolve) => setTimeout(resolve, 200));
      }
    } catch (error) {
      if (generation !== reloadGeneration) return;
      reloadError = error;
    } finally {
      if (generation === reloadGeneration) reloading = false;
    }
  }

  function startRollback(record) {
    result = null;
    sendRollback(record, {}, revision);
  }

  function confirmReview() {
    if (!review) return;
    const flags = ROLLBACK_CONFIRMATIONS.includes(review.required)
      ? { ...review.flags, [review.required]: true }
      : review.flags;
    sendRollback(review.record, flags, review.revision);
  }

  function formatTime(value) {
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? String(value ?? '') : date.toLocaleString();
  }

  // Load once on mount; later loads follow an explicit action only.
  $effect(() => {
    untrack(loadRecords);
  });
</script>

<section class="space-y-6">
  <div class="flex items-center justify-between">
    <PageHeader title={t('changes.title')} />
    <button
      type="button"
      onclick={loadRecords}
      class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
    >
      {t('common.refresh')}
    </button>
  </div>
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('changes.intro')}</p>

  <div class="space-y-3 rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800">
    <button
      type="button"
      onclick={reloadConfiguration}
      disabled={reloading}
      class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"
    >
      {reloading ? t('changes.reloading') : t('changes.reload')}
    </button>
    <p class="text-xs text-gray-500 dark:text-gray-400">{t('changes.reloadNotEveryone')}</p>
    {#if reloadError}
      <StateBlock variant={loadFailureVariant(reloadError)} message={reloadError?.message || t('changes.loadDenied')} />
    {:else if reloadResponse}
      <div class="space-y-1 text-sm">
        <p class="text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('changes.reloadResult')}</p>
        <p>{t('changes.reloadSuccess')}: {statusField(reloadResponse, 'success')}</p>
        <p>{t('changes.reloadMessage')}</p>
        <pre class="max-h-48 overflow-auto rounded-lg bg-gray-100 p-3 text-xs dark:bg-gray-900">{statusField(reloadResponse, 'message')}</pre>
      </div>
    {/if}
    {#if reloadStatus}
      <dl class="grid gap-1 text-sm sm:grid-cols-2">
        {#each RELOAD_FIELDS as [key, labelKey]}
          <dt class="text-gray-500 dark:text-gray-400">{t(labelKey)}</dt>
          <dd class="font-mono text-xs">{statusField(reloadStatus, key)}</dd>
        {/each}
      </dl>
    {/if}
  </div>

  <ErrorNotice error={actionError} />

  {#if review}
    <div class="space-y-3 rounded-xl border border-yellow-500/40 bg-white p-4 dark:bg-gray-800">
      <h3 class="text-base font-semibold">{t('changes.rollbackTitle', { record: review.record.id })}</h3>
      <ErrorNotice error={review.refusal} />
      {#if review.reasons.length > 0}
        <ul class="list-inside list-disc text-sm text-red-600 dark:text-red-300">
          {#each review.reasons as reason}
            <li class="font-mono text-xs">{riskReasonText(reason)}</li>
          {/each}
        </ul>
      {/if}
      <div>
        <p class="text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('changes.restorePlan')}</p>
        <pre class="max-h-72 overflow-auto rounded-lg bg-gray-100 p-3 text-xs dark:bg-gray-900">{review.plan}</pre>
      </div>
      <div>
        <p class="text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('changes.undoes')}</p>
        <pre class="max-h-72 overflow-auto rounded-lg bg-gray-100 p-3 text-xs dark:bg-gray-900">{review.undoes}</pre>
      </div>
      <p class="text-sm font-medium">{requiredText(review.required)}</p>
      <div class="flex justify-end gap-2">
        <button
          type="button"
          onclick={() => (review = null)}
          disabled={busy}
          class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 disabled:opacity-50 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
        >
          {t('manage.discard')}
        </button>
        <button
          type="button"
          onclick={confirmReview}
          disabled={busy}
          class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"
        >
          {busy ? t('manage.applying') : t('changes.confirmRollback')}
        </button>
      </div>
    </div>
  {/if}

  {#if result}
    <div class="space-y-2 rounded-lg border border-green-500/40 bg-green-500/10 px-3 py-2 text-sm">
      <p>{t('changes.rolledBack', { record: result.restores ?? '', status: result.status ?? '' })}</p>
      <p class="text-xs font-medium uppercase tracking-wide">{t('changes.restorePlan')}</p>
      <pre class="max-h-48 overflow-auto text-xs">{result.plan}</pre>
      <p class="text-xs font-medium uppercase tracking-wide">{t('changes.undoes')}</p>
      <pre class="max-h-48 overflow-auto text-xs">{result.undoes}</pre>
    </div>
  {/if}

  {#if loading}
    <StateBlock variant="loading" message={t('common.loading')} />
  {:else if loadError}
    <StateBlock variant={loadFailureVariant(loadError)} message={loadError?.message || t('changes.loadDenied')} />
  {:else if records.length === 0}
    <StateBlock variant="empty" message={t('changes.none')} />
  {:else}
    <div class="space-y-3">
      {#each records as record (record.id)}
        <article class="space-y-2 rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800">
          <div class="flex flex-wrap items-start justify-between gap-3">
            <div class="min-w-0 flex-1 space-y-1 text-sm">
              <p class="font-mono text-xs">{record.id}</p>
              <p>
                {formatTime(record.created_at)} · <span class="font-mono">{record.actor}</span> ·
                <span class="font-medium">{record.status}</span> · {record.summary?.risk}
              </p>
              {#if record.restores}
                <p class="text-xs text-gray-500 dark:text-gray-400">{t('changes.restores', { record: record.restores })}</p>
              {/if}
              {#if record.reason}
                <p class="text-xs text-red-600 dark:text-red-300">{record.reason}</p>
              {/if}
              {#if record.restart_required?.length > 0}
                <p class="text-xs text-gray-500 dark:text-gray-400">
                  {t('manage.restartRequired', { fields: record.restart_required.join(', ') })}
                </p>
              {/if}
            </div>
            <button
              type="button"
              onclick={() => startRollback(record)}
              disabled={busy}
              class="rounded-lg border border-yellow-500/50 bg-yellow-500/10 px-2 py-1 text-xs hover:bg-yellow-500/20 disabled:opacity-50"
            >
              {t('changes.rollback')}
            </button>
          </div>
          <details>
            <summary class="cursor-pointer text-xs text-gray-500 dark:text-gray-400">{t('changes.showPlan')}</summary>
            <pre class="mt-2 max-h-72 overflow-auto rounded-lg bg-gray-100 p-3 text-xs dark:bg-gray-900">{record.plan}</pre>
          </details>
        </article>
      {/each}
    </div>
  {/if}
</section>
