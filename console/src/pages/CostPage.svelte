<script>
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';
  import { operationFailure } from '../lib/operation-state';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import StateBlock from '../lib/ui/StateBlock.svelte';

  let windowName = $state('today');
  let phase = $state('loading');
  let message = $state('');
  let report = $state(null);
  let loadGeneration = 0;

  function finiteText(value) {
    return typeof value === 'number' && Number.isFinite(value) ? String(value) : t('common.unknown');
  }

  const selected = $derived(report?.selected && typeof report.selected === 'object' ? report.selected : null);

  async function load(name, ticket) {
    phase = 'loading';
    message = '';
    report = null;
    try {
      const body = await api.getCost(name);
      if (ticket !== loadGeneration) {
        return;
      }
      report = body && typeof body === 'object' ? body : null;
      phase = report ? 'ready' : 'error';
    } catch (error) {
      if (ticket !== loadGeneration) {
        return;
      }
      report = null;
      const failure = operationFailure(error);
      phase = failure.variant;
      message = failure.message;
    }
  }

  function choose(name) {
    windowName = name;
  }

  $effect(() => {
    const ticket = ++loadGeneration;
    load(windowName, ticket);
  });
</script>

<section class="space-y-4">
  <PageHeader title={t('operations.cost.title')} description={t('operations.cost.description')} />
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('operations.cost.scopeWorkspace')}</p>
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('operations.cost.estimates')}</p>
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('operations.cost.unpriced')}</p>
  <div class="flex flex-wrap gap-2" role="group" aria-label={t('operations.cost.window')}>
    {#each ['today', 'month', 'all'] as name}
      <button
        type="button"
        aria-pressed={windowName === name}
        onclick={() => choose(name)}
        class="rounded-lg border border-gray-300 px-3 py-2 text-sm {windowName === name ? 'bg-sky-600 text-white' : ''}"
      >
        {t(`operations.cost.${name}`)}
      </button>
    {/each}
  </div>
  {#if phase === 'loading'}
    <StateBlock variant="loading" message={t('operations.cost.loading')} />
  {:else if phase !== 'ready' || !report}
    <StateBlock variant={phase === 'ready' ? 'error' : phase} {message} />
  {:else}
    <dl class="grid gap-3 text-sm sm:grid-cols-2">
      <div>
        <dt>{t('operations.cost.scopeWorkspace')}</dt>
        <dd>{report.scope ?? t('common.unknown')}</dd>
      </div>
      <div>
        <dt>{t('operations.cost.settled')}</dt>
        <dd>{finiteText(selected?.cost_usd)}</dd>
      </div>
      <div>
        <dt>{t('operations.cost.reservations')}</dt>
        <dd>{finiteText(report.outstanding_reservation_micro_usd)}</dd>
      </div>
      <div>
        <dt>{t('operations.cost.unpriced')}</dt>
        <dd>{report.unpriced_usage === 'omitted' ? t('operations.cost.unpriced') : t('common.unknown')}</dd>
      </div>
      <div>
        <dt>{t('operations.cost.asOf')}</dt>
        <dd>{typeof report.as_of_utc === 'string' ? report.as_of_utc : t('common.unknown')}</dd>
      </div>
      <div>
        <dt>requests</dt>
        <dd>{finiteText(selected?.requests)}</dd>
      </div>
    </dl>
  {/if}
</section>
