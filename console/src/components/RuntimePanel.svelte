<script>
  import { ApiError, api } from '../lib/api';
  import { t } from '../lib/i18n';
  import ConfirmDialog from '../lib/ui/ConfirmDialog.svelte';
  import StateBlock from '../lib/ui/StateBlock.svelte';
  import StatusBadge from '../lib/ui/StatusBadge.svelte';

  let loading = $state(true);
  let failure = $state('');
  let variant = $state('loading');
  let running = $state([]);
  let unreaped = $state([]);
  let pools = $state(null);
  let poolsText = $state('');
  let rowNote = $state({});
  let pendingKill = $state(null);
  let pendingSteer = $state(null);
  let cascadeChoice = $state({});
  let steerDraft = $state({});
  let actionTicket = 0;

  function rowsOf(value) {
    return Array.isArray(value) ? value.filter((item) => item && typeof item.id === 'string') : [];
  }

  async function load() {
    loading = true;
    failure = '';
    const ticket = ++actionTicket;
    try {
      const [tasks, poolBody] = await Promise.all([api.listTasks(), api.getPools()]);
      if (ticket !== actionTicket) {
        return;
      }
      running = rowsOf(tasks?.running);
      unreaped = rowsOf(tasks?.unreaped);
      pools = poolBody ?? null;
      poolsText = JSON.stringify(poolBody ?? {}, null, 2);
      variant = '';
    } catch (error) {
      if (ticket !== actionTicket) {
        return;
      }
      running = [];
      unreaped = [];
      pools = null;
      if (error instanceof ApiError && error.status === 403) {
        variant = 'denied';
        failure = t('runtime.denied');
      } else if (!(error instanceof ApiError)) {
        variant = 'disconnected-stale';
        failure = t('chat.pollStale');
      } else {
        variant = 'error';
        failure = error.message || t('runtime.loadFailed');
      }
    } finally {
      if (ticket === actionTicket) {
        loading = false;
      }
    }
  }

  function askKill(item) {
    pendingKill = { ...item, cascade: cascadeChoice[item.id] !== false };
  }

  async function confirmKill() {
    const item = pendingKill;
    pendingKill = null;
    if (!item) {
      return;
    }
    const ticket = ++actionTicket;
    try {
      const receipt = await api.killTask(item.id, item.cascade !== false);
      if (ticket !== actionTicket) {
        return;
      }
      const targets = Array.isArray(receipt?.targets) ? receipt.targets.map((target) => `${target.id}:${target.outcome}`).join(', ') : '';
      rowNote = { ...rowNote, [item.id]: `${t('runtime.kill')}: ${targets || t('common.unknown')}` };
      await load();
    } catch (error) {
      if (ticket !== actionTicket) {
        return;
      }
      rowNote = {
        ...rowNote,
        [item.id]: error instanceof ApiError ? error.message : t('chat.pollStale')
      };
    }
  }

  function askSteer(item) {
    const message = (steerDraft[item.id] || '').trim();
    if (message.length === 0) {
      return;
    }
    pendingSteer = { ...item, message };
  }

  async function confirmSteer() {
    const item = pendingSteer;
    pendingSteer = null;
    if (!item) {
      return;
    }
    const ticket = ++actionTicket;
    try {
      const receipt = await api.steerTask(item.id, item.message);
      if (ticket !== actionTicket) {
        return;
      }
      const queued = receipt?.outcome === 'queued';
      rowNote = {
        ...rowNote,
        [item.id]: queued ? `${t('runtime.queued')}${receipt.note ? ` ${receipt.note}` : ''}` : t('runtime.queued')
      };
      steerDraft = { ...steerDraft, [item.id]: '' };
    } catch (error) {
      if (ticket !== actionTicket) {
        return;
      }
      if (error instanceof ApiError && error.status === 409) {
        rowNote = { ...rowNote, [item.id]: error.message || t('runtime.notSteerable') };
        return;
      }
      rowNote = {
        ...rowNote,
        [item.id]: error instanceof ApiError ? error.message : t('chat.pollStale')
      };
    }
  }

  $effect(() => {
    load();
  });
</script>

<div class="min-w-0 max-w-full space-y-4">
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('activity.processWide')}</p>
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('runtime.addressNote')}</p>
  {#if loading}
    <StateBlock variant="loading" message={t('common.loading')} />
  {:else if failure}
    <StateBlock {variant} message={failure} />
  {:else}
    {#if running.length === 0 && unreaped.length === 0}
      <StateBlock variant="empty" message={t('runtime.empty')} />
    {/if}
    {#each [{ title: t('runtime.running'), rows: running }, { title: t('runtime.unreaped'), rows: unreaped }] as group (group.title)}
      {#if group.rows.length > 0}
        <h3 class="text-lg font-semibold">{group.title}</h3>
        <ul class="min-w-0 space-y-3">
          {#each group.rows as item (item.id)}
            <li class="min-w-0 space-y-2 rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800">
              <p class="break-all font-mono text-xs">{item.id}</p>
              <p class="text-sm">{item.kind} · {item.name} · {item.state}</p>
              <p class="break-all text-sm">{t('runtime.parent')}: {item.parent || t('common.unknown')} · {t('runtime.pid')}: {item.pid ?? t('common.unknown')}</p>
              <p class="text-sm">
                {t('runtime.steerable')}:
                <StatusBadge label={item.steerable ? t('common.yes') : t('common.no')} tone={item.steerable ? 'success' : 'neutral'} />
              </p>
              {#if rowNote[item.id]}
                <p role="status" class="break-all text-sm">{rowNote[item.id]}</p>
              {/if}
              <label class="flex items-center gap-2 text-sm">
                <input
                  type="checkbox"
                  checked={cascadeChoice[item.id] !== false}
                  onchange={(event) => {
                    cascadeChoice = { ...cascadeChoice, [item.id]: event.currentTarget.checked };
                  }}
                />
                {t('runtime.cascade')}
              </label>
              <div class="flex flex-wrap gap-2">
                <button type="button" class="rounded-lg border border-amber-700 px-3 py-2 text-sm" onclick={() => askKill(item)}>
                  {t('runtime.kill')}
                </button>
              </div>
              <label class="block text-sm">
                <span class="mb-1 block">{t('runtime.message')}</span>
                <textarea
                  class="min-h-16 w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm dark:border-gray-600 dark:bg-gray-900"
                  value={steerDraft[item.id] || ''}
                  oninput={(event) => {
                    steerDraft = { ...steerDraft, [item.id]: event.currentTarget.value };
                  }}
                ></textarea>
              </label>
              <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm text-white" onclick={() => askSteer(item)}>
                {t('runtime.send')}
              </button>
            </li>
          {/each}
        </ul>
      {/if}
    {/each}
    <h3 class="text-lg font-semibold">{t('runtime.pools')}</h3>
    <pre class="max-w-full overflow-x-auto whitespace-pre-wrap break-all rounded-xl border border-gray-200 bg-white p-3 text-xs dark:border-gray-700 dark:bg-gray-900">{poolsText}</pre>
  {/if}
</div>

<ConfirmDialog
  open={pendingKill !== null}
  title={t('runtime.killTitle')}
  objectName={pendingKill?.id || ''}
  consequence={t('runtime.killConsequence')}
  onCancel={() => {
    pendingKill = null;
  }}
  onConfirm={confirmKill}
/>
<ConfirmDialog
  open={pendingSteer !== null}
  title={t('runtime.steerTitle')}
  objectName={pendingSteer?.id || ''}
  consequence={t('runtime.steerConsequence')}
  onCancel={() => {
    pendingSteer = null;
  }}
  onConfirm={confirmSteer}
/>
