<script>
  import ErrorNotice from './ErrorNotice.svelte';
  import { riskReasonText } from './change-flow.svelte.js';
  import { t } from './i18n';

  // `flow`: a ChangeFlow. Shows the dry run of the pending change (risk,
  // reasons, the redacted diff and the plan text), applies or discards it,
  // and reports the outcome of the last apply.
  let { flow } = $props();

  function shown(value) {
    return value === undefined || value === null ? '—' : JSON.stringify(value);
  }
</script>

<ErrorNotice error={flow.error} />

{#if flow.pending}
  {@const plan = flow.pending.plan}
  <div class="space-y-3 rounded-xl border border-sky-500/30 bg-white p-4 dark:bg-gray-800">
    <div class="flex flex-wrap items-center gap-2">
      <h3 class="text-base font-semibold">{t('manage.planTitle')}</h3>
      <span
        class={`rounded-full px-2 py-0.5 text-xs font-medium ${
          plan.risk === 'high'
            ? 'border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300'
            : 'border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300'
        }`}
      >
        {plan.risk === 'high' ? t('manage.riskHigh') : t('manage.riskLow')}
      </span>
    </div>

    {#if Array.isArray(plan.risk_reasons) && plan.risk_reasons.length > 0}
      <ul class="list-inside list-disc text-sm text-red-600 dark:text-red-300">
        {#each plan.risk_reasons as reason}
          <li class="font-mono text-xs">{riskReasonText(reason)}</li>
        {/each}
      </ul>
    {/if}

    {#if Array.isArray(plan.changes) && plan.changes.length > 0}
      <div class="overflow-x-auto">
        <table class="w-full text-left text-xs">
          <thead class="text-gray-500 dark:text-gray-400">
            <tr>
              <th class="py-1 pr-3">{t('manage.path')}</th>
              <th class="py-1 pr-3">{t('manage.change')}</th>
              <th class="py-1 pr-3">{t('manage.before')}</th>
              <th class="py-1">{t('manage.after')}</th>
            </tr>
          </thead>
          <tbody class="font-mono">
            {#each plan.changes as entry}
              <tr class="border-t border-gray-200 dark:border-gray-700">
                <td class="py-1 pr-3">{entry.path}</td>
                <td class="py-1 pr-3">{entry.kind}</td>
                <td class="py-1 pr-3 break-all">{shown(entry.before)}</td>
                <td class="py-1 break-all">{shown(entry.after)}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}

    {#if Array.isArray(plan.unverified) && plan.unverified.length > 0}
      <div class="rounded-lg border border-yellow-500/40 bg-yellow-500/10 px-3 py-2 text-xs">
        <p class="font-medium">{t('manage.unverified')}</p>
        {#each plan.unverified as warning}
          <p>{warning}</p>
        {/each}
      </div>
    {/if}

    <pre class="max-h-72 overflow-auto rounded-lg bg-gray-100 p-3 text-xs dark:bg-gray-900">{plan.plan}</pre>

    <div class="flex justify-end gap-2">
      <button
        type="button"
        onclick={() => flow.cancel()}
        disabled={flow.busy}
        class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 disabled:opacity-50 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
      >
        {t('manage.discard')}
      </button>
      <button
        type="button"
        onclick={() => flow.apply()}
        disabled={flow.busy}
        class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"
      >
        {flow.busy ? t('manage.applying') : t('manage.apply')}
      </button>
    </div>
  </div>
{/if}

{#if flow.result}
  <div class="space-y-1 rounded-lg border border-green-500/40 bg-green-500/10 px-3 py-2 text-sm">
    <p>{t('manage.applied', { status: flow.result.status ?? '', record: flow.result.record_id ?? '' })}</p>
    {#if Array.isArray(flow.result.restart_required) && flow.result.restart_required.length > 0}
      <p>{t('manage.restartRequired', { fields: flow.result.restart_required.join(', ') })}</p>
    {/if}
    {#if flow.result.warning}
      <p>{flow.result.warning}</p>
    {/if}
  </div>
{/if}
