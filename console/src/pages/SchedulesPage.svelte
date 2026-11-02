<script>
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';
  import { operationFailure } from '../lib/operation-state';
  import { navigate } from '../lib/router';
  import ConfirmDialog from '../lib/ui/ConfirmDialog.svelte';
  import DataTable from '../lib/ui/DataTable.svelte';
  import Field from '../lib/ui/Field.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import StateBlock from '../lib/ui/StateBlock.svelte';

  let { path = '/schedules' } = $props();

  let jobs = $state([]);
  let detail = $state(null);
  let runs = $state([]);
  let phase = $state('loading');
  let message = $state('');
  let notice = $state(null);
  let runResult = $state(null);
  let kind = $state('expression');
  let expression = $state('');
  let at = $state('');
  let every = $state('');
  let once = $state('');
  let command = $state('');
  let jobName = $state('');
  let timezone = $state('');
  let formError = $state('');
  let removeOpen = $state(false);
  let loadGeneration = 0;

  const jobId = $derived.by(() => {
    if (typeof path !== 'string' || !path.startsWith('/schedules/')) {
      return '';
    }
    const raw = path.slice('/schedules/'.length);
    if (raw.length === 0 || raw.includes('/')) {
      return '';
    }
    try {
      return decodeURIComponent(raw);
    } catch {
      return raw;
    }
  });

  function scheduleText(schedule) {
    if (!schedule || typeof schedule !== 'object') {
      return '';
    }
    if (schedule.kind === 'cron' && typeof schedule.expr === 'string') {
      return schedule.expr;
    }
    if (schedule.kind === 'at' && typeof schedule.at === 'string') {
      return schedule.at;
    }
    if (schedule.kind === 'every' && typeof schedule.every_ms === 'number') {
      return String(schedule.every_ms);
    }
    return '';
  }

  function jobRows(list) {
    return list.map((job) => ({
      id: typeof job?.id === 'string' ? job.id : '',
      name: typeof job?.name === 'string' ? job.name : '',
      schedule: scheduleText(job?.schedule),
      command: typeof job?.command === 'string' ? job.command : '',
      enabled: job?.enabled === true
    }));
  }

  function runRows(list) {
    return list.map((run) => ({
      id: typeof run?.id === 'string' ? run.id : '',
      status: typeof run?.status === 'string' ? run.status : '',
      started_at: typeof run?.started_at === 'string' ? run.started_at : '',
      output: typeof run?.output === 'string' ? run.output : ''
    }));
  }

  function eventRows(list) {
    return (Array.isArray(list) ? list : []).map((event) => ({
      event_type: typeof event?.event_type === 'string' ? event.event_type : '',
      status: typeof event?.status === 'string' ? event.status : '',
      created_at: typeof event?.created_at === 'string' ? event.created_at : ''
    }));
  }

  function fail(error) {
    const failure = operationFailure(error);
    phase = failure.variant;
    message = failure.message;
    notice = null;
  }

  async function refresh(id, ticket) {
    phase = 'loading';
    message = '';
    notice = null;
    try {
      if (id) {
        const [job, runBody] = await Promise.all([api.getCronJob(id), api.listCronRuns(id)]);
        if (ticket !== loadGeneration) {
          return;
        }
        detail = job;
        runs = Array.isArray(runBody?.runs) ? runBody.runs : [];
      } else {
        const [listed, statusBody] = await Promise.all([api.listCronJobs(), api.getCronStatus()]);
        if (ticket !== loadGeneration) {
          return;
        }
        jobs = Array.isArray(listed?.jobs) ? listed.jobs : [];
        detail = statusBody;
      }
      phase = 'ready';
    } catch (error) {
      if (ticket !== loadGeneration) {
        return;
      }
      fail(error);
    }
  }

  function reload() {
    const ticket = ++loadGeneration;
    refresh(jobId, ticket);
  }

  $effect(() => {
    const id = jobId;
    const ticket = ++loadGeneration;
    refresh(id, ticket);
  });

  function buildCreateBody() {
    const payload = { command: command.trim() };
    if (jobName.trim()) {
      payload.name = jobName.trim();
    }
    if (kind === 'expression') {
      payload.expression = expression.trim();
      if (timezone.trim()) {
        payload.tz = timezone.trim();
      }
    } else if (kind === 'at') {
      payload.at = at.trim();
    } else if (kind === 'every') {
      const milliseconds = Number(every);
      if (!Number.isInteger(milliseconds) || milliseconds <= 0) {
        return null;
      }
      payload.every_ms = milliseconds;
    } else {
      payload.delay = once.trim();
    }
    const hasSchedule = payload.expression || payload.at || payload.every_ms || payload.delay;
    if (!payload.command || !hasSchedule) {
      return null;
    }
    return payload;
  }

  async function addJob(event) {
    event.preventDefault();
    const payload = buildCreateBody();
    if (!payload) {
      formError = t('operations.schedules.required');
      return;
    }
    formError = '';
    notice = null;
    try {
      await api.createCronJob(payload);
      command = '';
      expression = '';
      at = '';
      every = '';
      once = '';
      jobName = '';
      timezone = '';
      reload();
    } catch (error) {
      notice = operationFailure(error);
    }
  }

  async function mutate(action) {
    if (!jobId) {
      return;
    }
    notice = null;
    try {
      if (action === 'pause') {
        detail = await api.pauseCronJob(jobId);
      } else if (action === 'resume') {
        detail = await api.resumeCronJob(jobId);
      } else if (action === 'remove') {
        await api.deleteCronJob(jobId);
        removeOpen = false;
        navigate('/schedules');
        return;
      }
    } catch (error) {
      notice = operationFailure(error);
    }
  }

  async function updateJob(event) {
    event.preventDefault();
    if (!jobId) {
      return;
    }
    const payload = buildCreateBody();
    if (!payload) {
      formError = t('operations.schedules.required');
      return;
    }
    formError = '';
    notice = null;
    try {
      detail = await api.updateCronJob(jobId, payload);
    } catch (error) {
      notice = operationFailure(error);
    }
  }

  async function runNow() {
    if (!jobId) {
      return;
    }
    notice = null;
    runResult = null;
    try {
      const result = await api.runCronJob(jobId);
      runResult = result && typeof result === 'object' ? result : {};
      const runBody = await api.listCronRuns(jobId);
      runs = Array.isArray(runBody?.runs) ? runBody.runs : [];
    } catch (error) {
      const failure = operationFailure(error);
      if (error?.status === 409) {
        notice = { variant: 'error', message: failure.message || t('operations.schedules.conflict') };
        return;
      }
      notice = failure;
    }
  }
</script>

<section class="space-y-4">
  <PageHeader title={t('operations.schedules.title')} description={t('operations.schedules.description')} />
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('operations.schedules.markOperator')}</p>
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('operations.schedules.shellNote')}</p>
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('operations.schedules.disabledGap')}</p>
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('operations.schedules.noRunId')}</p>

  {#if phase === 'loading'}
    <StateBlock variant="loading" message={t('operations.schedules.loading')} />
  {:else if phase !== 'ready'}
    <StateBlock variant={phase} {message} />
  {:else if jobId}
    <button
      type="button"
      onclick={() => navigate('/schedules')}
      class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200"
    >
      {t('operations.schedules.back')}
    </button>
    <h3 class="break-all font-mono text-sm">{detail?.id ?? jobId}</h3>
    <p class="text-sm">{scheduleText(detail?.schedule) || t('common.unknown')}</p>
    <div class="flex flex-wrap gap-2">
      {#if detail?.enabled === true}
        <button type="button" onclick={() => mutate('pause')} class="rounded-lg border border-gray-300 px-3 py-2 text-sm">{t('operations.schedules.pause')}</button>
      {:else}
        <button type="button" onclick={() => mutate('resume')} class="rounded-lg border border-gray-300 px-3 py-2 text-sm">{t('operations.schedules.resume')}</button>
      {/if}
      <button type="button" onclick={runNow} class="rounded-lg bg-sky-600 px-3 py-2 text-sm text-white">{t('operations.schedules.run')}</button>
      <button type="button" onclick={() => (removeOpen = true)} class="rounded-lg border border-red-400 px-3 py-2 text-sm">{t('operations.schedules.remove')}</button>
    </div>
    <form class="space-y-3" onsubmit={updateJob}>
      <Field name="command" label={t('operations.schedules.command')} value={command} onValue={(value) => (command = value)} />
      <Field name="expression" label={t('operations.schedules.expression')} value={expression} onValue={(value) => (expression = value)} />
      <button type="submit" class="rounded-lg border border-gray-300 px-3 py-2 text-sm">{t('common.save')}</button>
    </form>
    {#if runResult}
      <dl class="grid gap-2 text-sm sm:grid-cols-2">
        <div><dt>job_id</dt><dd class="font-mono">{runResult.job_id ?? t('common.unknown')}</dd></div>
        <div><dt>status</dt><dd>{runResult.status ?? t('common.unknown')}</dd></div>
        <div><dt>duration_ms</dt><dd>{runResult.duration_ms ?? t('common.unknown')}</dd></div>
        <div><dt>output</dt><dd class="whitespace-pre-wrap break-words">{runResult.output || t('common.empty')}</dd></div>
      </dl>
    {/if}
    <h3 class="text-sm font-semibold">{t('operations.schedules.runs')}</h3>
    <DataTable
      caption={t('operations.schedules.runs')}
      columns={[
        { key: 'id', label: 'id', truncate: true },
        { key: 'status', label: 'status' },
        { key: 'started_at', label: 'started_at' },
        { key: 'output', label: 'output', truncate: true }
      ]}
      rows={runRows(runs)}
    />
    <h3 class="text-sm font-semibold">{t('operations.schedules.events')}</h3>
    <DataTable
      caption={t('operations.schedules.events')}
      columns={[
        { key: 'event_type', label: 'event_type' },
        { key: 'status', label: 'status' },
        { key: 'created_at', label: 'created_at' }
      ]}
      rows={eventRows(detail?.events)}
    />
  {:else}
    <DataTable
      caption={t('operations.schedules.title')}
      columns={[
        { key: 'name', label: t('operations.schedules.name') },
        { key: 'schedule', label: t('operations.schedules.kind') },
        { key: 'command', label: t('operations.schedules.command'), truncate: true },
        { key: 'enabled', label: t('common.enabled') }
      ]}
      rows={jobRows(jobs)}
    />
    <ul class="flex flex-wrap gap-2">
      {#each jobs as job (job.id)}
        <li>
          <button
            type="button"
            onclick={() => navigate(`/schedules/${encodeURIComponent(job.id)}`)}
            class="rounded-lg border border-gray-300 px-3 py-2 text-sm"
          >
            {t('operations.schedules.open')} {job.name || job.id}
          </button>
        </li>
      {/each}
    </ul>
    <form class="space-y-3" onsubmit={addJob}>
      <div>
        <label for="schedule-kind">{t('operations.schedules.kind')}</label>
        <select id="schedule-kind" bind:value={kind} class="mt-1 block w-full max-w-md rounded border border-gray-400 bg-transparent p-2">
          <option value="expression">{t('operations.schedules.expression')}</option>
          <option value="at">{t('operations.schedules.at')}</option>
          <option value="every">{t('operations.schedules.every')}</option>
          <option value="once">{t('operations.schedules.once')}</option>
        </select>
      </div>
      {#if kind === 'expression'}
        <Field name="expression" label={t('operations.schedules.expression')} value={expression} onValue={(value) => (expression = value)} />
        <Field name="timezone" label={t('operations.schedules.timezone')} value={timezone} onValue={(value) => (timezone = value)} />
      {:else if kind === 'at'}
        <Field name="at" label={t('operations.schedules.at')} value={at} onValue={(value) => (at = value)} />
      {:else if kind === 'every'}
        <Field name="every" label={t('operations.schedules.every')} value={every} onValue={(value) => (every = value)} />
      {:else}
        <Field name="once" label={t('operations.schedules.once')} value={once} onValue={(value) => (once = value)} />
      {/if}
      <Field name="command" label={t('operations.schedules.command')} value={command} onValue={(value) => (command = value)} />
      <Field name="job-name" label={t('operations.schedules.name')} value={jobName} onValue={(value) => (jobName = value)} />
      {#if formError}
        <p class="ui-field-error" role="alert">{formError}</p>
      {/if}
      <button type="submit" class="rounded-lg bg-sky-600 px-3 py-2 text-sm text-white">{t('operations.schedules.add')}</button>
    </form>
  {/if}

  {#if notice}
    <StateBlock variant={notice.variant} message={notice.message} />
  {/if}

  <ConfirmDialog
    open={removeOpen}
    title={t('operations.schedules.removeTitle')}
    objectName={detail?.name || jobId || t('common.unknown')}
    consequence={t('operations.schedules.removeConsequence')}
    confirmLabel={t('operations.schedules.remove')}
    onConfirm={() => mutate('remove')}
    onCancel={() => (removeOpen = false)}
  />
</section>
