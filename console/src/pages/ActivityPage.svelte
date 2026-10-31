<script>
  import { ApiError, api } from '../lib/api';
  import { formatUptime } from '../lib/format';
  import { t } from '../lib/i18n';
  import ConfirmDialog from '../lib/ui/ConfirmDialog.svelte';
  import DataTable from '../lib/ui/DataTable.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import StateBlock from '../lib/ui/StateBlock.svelte';
  import StatusBadge from '../lib/ui/StatusBadge.svelte';
  import RuntimePanel from '../components/RuntimePanel.svelte';

  const PHASES = ['all', 'running', 'succeeded', 'failed', 'cancelled'];
  let tab = $state('jobs');
  let phase = $state('all');
  let loading = $state(true);
  let failure = $state('');
  let variant = $state('loading');
  let jobs = $state([]);
  let selectedId = $state('');
  let detail = $state(null);
  let detailError = $state('');
  let cancelOpen = $state(false);
  let cancelAck = $state('');
  let detailTicket = 0;

  const phaseTones = {
    accepted: 'info',
    running: 'info',
    succeeded: 'success',
    failed: 'danger',
    cancelled: 'warning'
  };

  function phaseLabel(status) {
    if (typeof status !== 'string' || status.length === 0) {
      return t('common.unknown');
    }
    const key = `chat.${status}`;
    const translated = t(key);
    return translated === key ? status : translated;
  }

  function elapsedLabel(ms) {
    const value = Number(ms);
    if (!Number.isFinite(value) || value < 0) {
      return t('common.unknown');
    }
    return formatUptime(value / 1000);
  }

  const columns = $derived([
    { key: 'job_id', label: t('chat.job'), truncate: true },
    { key: 'status', label: t('activity.status') },
    { key: 'label', label: t('activity.label'), truncate: true },
    { key: 'work_id', label: t('activity.workId'), truncate: true },
    { key: 'elapsed_ms', label: t('activity.elapsed') }
  ]);

  const visible = $derived(
    jobs.filter((job) => phase === 'all' || job.status === phase).map((job) => ({
      job_id: job.job_id,
      status: phaseLabel(job.status),
      label: job.label || '',
      work_id: job.work_id || '',
      elapsed_ms: elapsedLabel(job.elapsed_ms)
    }))
  );

  async function loadJobs() {
    loading = true;
    failure = '';
    try {
      const response = await api.listJobs();
      const rows = Array.isArray(response?.jobs) ? response.jobs : null;
      if (!rows) {
        throw new Error(t('activity.loadFailed'));
      }
      jobs = rows.filter((job) => job && typeof job.job_id === 'string');
      variant = '';
      if (selectedId && !jobs.some((job) => job.job_id === selectedId)) {
        selectedId = '';
        detail = null;
        cancelAck = '';
      }
    } catch (error) {
      jobs = [];
      if (error instanceof ApiError && error.status === 403) {
        variant = 'denied';
        failure = t('activity.denied');
      } else if (!(error instanceof ApiError)) {
        variant = 'disconnected-stale';
        failure = t('common.connectionLost');
      } else {
        variant = 'error';
        failure = error.message || t('activity.loadFailed');
      }
    } finally {
      loading = false;
    }
  }

  async function loadDetail(jobId) {
    const ticket = ++detailTicket;
    detailError = '';
    try {
      const snapshot = await api.getJob(jobId);
      if (ticket !== detailTicket || selectedId !== jobId) {
        return;
      }
      detail = snapshot;
    } catch (error) {
      if (ticket !== detailTicket || selectedId !== jobId) {
        return;
      }
      detail = null;
      detailError = error instanceof ApiError ? error.message : t('common.connectionLost');
    }
  }

  function chooseJob(event) {
    selectedId = event.currentTarget.value;
    detail = null;
    cancelAck = '';
    if (selectedId) {
      loadDetail(selectedId);
    }
  }

  async function confirmCancel() {
    const jobId = selectedId;
    cancelOpen = false;
    if (!jobId) {
      return;
    }
    const ticket = ++detailTicket;
    try {
      const receipt = await api.cancelJob(jobId);
      if (ticket !== detailTicket || selectedId !== jobId) {
        return;
      }
      const targets = Array.isArray(receipt?.targets) ? receipt.targets.join(', ') : '';
      cancelAck = `${t('activity.cancelAck')} ${receipt?.job_id || jobId} ${receipt?.work_id || ''} ${targets}`.trim();
      await loadDetail(jobId);
    } catch (error) {
      if (ticket !== detailTicket) {
        return;
      }
      detailError = error instanceof ApiError ? error.message : t('runtime.outcomeUnknown');
    }
  }

  function onTabKey(event) {
    const order = ['jobs', 'runtime'];
    const index = order.indexOf(tab);
    if (event.key !== 'ArrowRight' && event.key !== 'ArrowLeft') {
      return;
    }
    event.preventDefault();
    const next = event.key === 'ArrowRight' ? (index + 1) % order.length : (index - 1 + order.length) % order.length;
    tab = order[next];
    const button = document.getElementById(`activity-tab-${tab}`);
    button?.focus();
  }

  $effect(() => {
    if (tab === 'jobs') {
      loadJobs();
    }
  });
</script>

<section class="min-w-0 max-w-full space-y-4">
  <PageHeader title={t('activity.title')} description={t('activity.description')} />
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('activity.processWide')}</p>
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('activity.addressNote')}</p>
  <div role="tablist" aria-label={t('activity.title')} class="flex flex-wrap gap-2">
    <button
      type="button"
      role="tab"
      id="activity-tab-jobs"
      aria-selected={tab === 'jobs'}
      aria-controls="activity-panel-jobs"
      tabindex={tab === 'jobs' ? 0 : -1}
      class="rounded-lg border px-3 py-2 text-sm"
      onclick={() => { tab = 'jobs'; }}
      onkeydown={onTabKey}
    >
      {t('activity.jobs')}
    </button>
    <button
      type="button"
      role="tab"
      id="activity-tab-runtime"
      aria-selected={tab === 'runtime'}
      aria-controls="activity-panel-runtime"
      tabindex={tab === 'runtime' ? 0 : -1}
      class="rounded-lg border px-3 py-2 text-sm"
      onclick={() => { tab = 'runtime'; }}
      onkeydown={onTabKey}
    >
      {t('activity.runtime')}
    </button>
  </div>

  {#if tab === 'jobs'}
    <div role="tabpanel" id="activity-panel-jobs" aria-labelledby="activity-tab-jobs" class="min-w-0 space-y-4">
      <label class="block max-w-full text-sm">
        <span class="mb-1 block">{t('activity.phase')}</span>
        <select class="w-full max-w-xs rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm dark:border-gray-600 dark:bg-gray-900" bind:value={phase}>
          {#each PHASES as option}
            <option value={option}>{option === 'all' ? t('activity.allPhases') : t(`chat.${option}`)}</option>
          {/each}
        </select>
      </label>
      {#if loading}
        <StateBlock variant="loading" message={t('common.loading')} />
      {:else if failure}
        <StateBlock {variant} message={failure} />
      {:else if visible.length === 0}
        <StateBlock variant="empty" message={t('activity.empty')} />
      {:else}
        <DataTable {columns} rows={visible} caption={t('activity.jobs')} />
        <label class="block max-w-full text-sm">
          <span class="mb-1 block">{t('activity.selectJob')}</span>
          <select class="w-full max-w-xl rounded-lg border border-gray-300 bg-white px-3 py-2 font-mono text-xs dark:border-gray-600 dark:bg-gray-900" onchange={chooseJob}>
            <option value="">{t('activity.selectJob')}</option>
            {#each jobs as job (job.job_id)}
              <option value={job.job_id}>{job.job_id}</option>
            {/each}
          </select>
        </label>
        {#if detailError}
          <StateBlock variant="error" message={detailError} />
        {/if}
        {#if detail}
          <article class="min-w-0 space-y-2 rounded-xl border border-gray-200 p-4 dark:border-gray-700">
            <h3 class="text-lg font-semibold">{t('activity.detail')}</h3>
            <StatusBadge label={phaseLabel(detail.status)} tone={phaseTones[detail.status] || 'neutral'} />
            <p class="text-sm">{t('activity.elapsed')}: {elapsedLabel(detail.elapsed_ms)}</p>
            <p class="break-all text-sm">{t('activity.httpStatus')}: {detail.http_status ?? t('common.unknown')}</p>
            <p class="break-all font-mono text-xs">{t('activity.workId')}: {detail.work_id || t('common.unknown')}</p>
            <p class="text-sm">{t('activity.result')}</p>
            <pre class="max-w-full overflow-x-auto whitespace-pre-wrap break-all text-xs">{JSON.stringify(detail.result ?? null, null, 2)}</pre>
            <p class="text-sm">{t('activity.error')}</p>
            <pre class="max-w-full overflow-x-auto whitespace-pre-wrap break-all text-xs">{typeof detail.error === 'string' ? detail.error : JSON.stringify(detail.error ?? null)}</pre>
            {#if cancelAck}
              <p role="status" class="break-all text-sm">{cancelAck}</p>
            {/if}
            {#if detail.status === 'running'}
              <button type="button" class="rounded-lg border border-amber-700 px-3 py-2 text-sm" onclick={() => { cancelOpen = true; }}>
                {t('chat.cancelTurn')}
              </button>
            {/if}
          </article>
        {/if}
      {/if}
    </div>
  {:else}
    <div role="tabpanel" id="activity-panel-runtime" aria-labelledby="activity-tab-runtime">
      <RuntimePanel />
    </div>
  {/if}
</section>

<ConfirmDialog
  open={cancelOpen}
  title={t('activity.cancelTitle')}
  objectName={selectedId}
  consequence={t('activity.cancelConsequence')}
  onCancel={() => { cancelOpen = false; }}
  onConfirm={confirmCancel}
/>
