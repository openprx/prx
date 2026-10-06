<script>
  import { onDestroy } from 'svelte';
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';
  let { disabled = false, oncalibrate } = $props();
  let from = $state(new Date(Date.now() - 7 * 86400000).toISOString().slice(0, 10));
  let to = $state(new Date().toISOString().slice(0, 10));
  let reportId = $state('');
  let report = $state(null);
  let saved = $state(null);
  let error = $state('');
  let busy = $state(false);
  let alive = true;
  onDestroy(() => { alive = false; });
  // Only named numeric metrics are rendered; arbitrary future report fields
  // (including backend endpoint or prompt material) are not projected.
  const groups = {
    coverage: ['decisions','complete_outcomes','incomplete_outcomes','without_outcome','legacy_excluded','coverage'],
    rates: ['outcomes','success_rate','fallback_rate','retry_rate'],
    latency_ms: ['p50','p95','p99'],
    backend: ['failure_rate','circuit_rate','builtin_rate'],
    bytes: ['outcomes','prompt_before','prompt_after','tool_before','tool_after'],
    next_action: ['compared','accuracy'],
    capability_target: ['compared','accuracy'],
    next_action_calibration: ['samples','brier','ece'],
    capability_target_calibration: ['samples','brier','ece']
  };
  const number = (v) => typeof v === 'number' && Number.isFinite(v) ? v : null;
  function projection(source) {
    const metrics = {};
    for (const [group, fields] of Object.entries(groups)) {
      metrics[group] = Object.fromEntries(fields.map(key => [key, number(source[group]?.[key])]));
    }
    const proposal = source.calibration?.status === 'available' ? source.calibration.value : null;
    return {
      report_id: source.report_id, metrics,
      cost: number(source.cost_per_successful_turn_micro_usd),
      calibration: proposal ? {
        decisions: number(proposal.decisions),
        confidence: number(proposal.minimum_choice_confidence),
        margin: number(proposal.minimum_top_margin)
      } : null,
      reason: typeof source.calibration?.reason === 'string' ? source.calibration.reason : '—',
      counterfactual: source.counterfactual?.status === 'available' && Array.isArray(source.counterfactual.value)
        ? source.counterfactual.value.map(v => ({ action:v.action, target:v.target, ess:number(v.ess), ips:number(v.ips), dr:number(v.doubly_robust), low:number(v.ci95?.[0]), high:number(v.ci95?.[1]) })) : [],
      counterfactualReason: typeof source.counterfactual?.reason === 'string' ? source.counterfactual.reason : ''
    };
  }
  async function load(event) {
    event.preventDefault();
    if (busy || disabled) return;
    error = ''; report = null;
    if (!from || !to || from > to) { error = t('decisionReport.invalidWindow'); return; }
    busy = true;
    try {
      const query = { from, to };
      const response = await api.getDecisionReport(query);
      if (alive) report = { ...projection(response), window: query };
    } catch (failure) {
      if (alive) error = failure instanceof Error ? failure.message : t('decisionInspect.invalidResponse');
    } finally { if (alive) busy = false; }
  }
  async function loadSaved() {
    if (busy || disabled) return;
    busy = true; error = ''; saved = null;
    try {
      const response = await api.getSavedDecisionReports();
      if (!Array.isArray(response.reports) || typeof response.complete !== 'boolean') {
        throw new Error(t('decisionInspect.invalidResponse'));
      }
      const reports = response.reports.map(item => {
        if (typeof item.report_id !== 'string' || !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(item.report_id)
          || ![item.created_at_ms, item.from_ms, item.to_ms].every(Number.isSafeInteger)) {
          throw new Error(t('decisionInspect.invalidResponse'));
        }
        return { report_id: item.report_id, created_at_ms: item.created_at_ms, from_ms: item.from_ms, to_ms: item.to_ms };
      });
      if (alive) saved = { reports, complete: response.complete };
    } catch {
      if (alive) error = t('decisionInspect.invalidResponse');
    } finally { if (alive) busy = false; }
  }
  function utc(ms) {
    const date = new Date(ms);
    return Number.isNaN(date.getTime()) ? '—' : date.toISOString();
  }
  function calibrate(event) {
    event.preventDefault();
    if (busy || disabled) return;
    const id = reportId.trim();
    if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(id)) { error = t('decisionApproval.invalidId'); return; }
    error = ''; oncalibrate(id);
  }
</script>

<section class="space-y-3 rounded-xl border border-gray-200 p-4 dark:border-gray-700">
  <h3 class="font-semibold">{t('decisionReport.title')}</h3>
  <p class="text-sm">{t('decisionReport.meaning')}</p>
  <form onsubmit={load} class="flex flex-wrap items-end gap-3">
    <label>{t('decisionInspect.from')}<input type="date" bind:value={from} disabled={busy || disabled} class="mt-1 block rounded border border-gray-400 bg-transparent p-2" /></label>
    <label>{t('decisionInspect.to')}<input type="date" bind:value={to} disabled={busy || disabled} class="mt-1 block rounded border border-gray-400 bg-transparent p-2" /></label>
    <button disabled={busy || disabled} class="rounded-lg border border-gray-400 px-3 py-2 disabled:opacity-50">{t('decisionReport.load')}</button>
  </form>
  {#if busy}<p aria-live="polite">{t('common.loading')}</p>{/if}
  {#if report}
    <p class="text-sm">{t('decisionReport.loadedWindow', report.window)}</p>
    <p class="break-all font-mono text-xs">{report.report_id}</p>
    <p class="text-sm">{t('decisionReport.readOnly')}</p>
    <div class="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
      {#each Object.entries(report.metrics) as [group, metrics]}
        <div class="rounded-lg bg-gray-100 p-3 dark:bg-gray-800">
          <h4 class="font-semibold">{t(`decisionReport.${group}`)}</h4>
          <dl class="mt-2 space-y-1 text-sm">{#each Object.entries(metrics) as [key, value]}<div class="flex justify-between gap-3"><dt>{t(`reportMetrics.${key}`)}</dt><dd>{value ?? '—'}</dd></div>{/each}</dl>
        </div>
      {/each}
    </div>
    <p>{t('decisionReport.cost')}: {report.cost ?? '—'}</p>
    <h4 class="font-semibold">{t('decisionReport.calibration')}</h4>
    {#if report.calibration}
      <p>{t('decisionReport.proposal', report.calibration)}</p>
    {:else}<p>{t('decisionReport.unavailable')}: {report.reason}</p>{/if}
    <h4 class="font-semibold">{t('decisionReport.counterfactual')}</h4>
    {#if report.counterfactualReason}<p>{t('decisionReport.unavailable')}: {report.counterfactualReason}</p>{/if}
    {#each report.counterfactual as item}<p class="text-sm">{item.action} / {item.target}: ESS {item.ess ?? '—'}, IPS {item.ips ?? '—'}, DR {item.dr ?? '—'}, 95% CI [{item.low ?? '—'}, {item.high ?? '—'}]</p>{/each}
  {/if}
  <div class="space-y-2 border-t border-gray-200 pt-3 dark:border-gray-700">
    <button type="button" onclick={loadSaved} disabled={busy || disabled} class="rounded-lg border border-gray-400 px-3 py-2 disabled:opacity-50">{t('reportCatalog.load')}</button>
    <p class="text-sm">{t('reportCatalog.scope')}</p>
    {#if saved}
      {#if !saved.complete}<p role="status">{t('reportCatalog.partial')}</p>{/if}
      {#if saved.reports.length === 0}<p>{t('reportCatalog.empty')}</p>{/if}
      <ul class="space-y-2">
        {#each saved.reports as item (item.report_id)}
          <li class="rounded border border-gray-300 p-2 dark:border-gray-600">
            <p class="break-all font-mono text-xs">{item.report_id}</p>
            <p class="text-xs">{utc(item.from_ms)} – {utc(item.to_ms)}</p>
            <button type="button" disabled={busy || disabled} onclick={() => { reportId = item.report_id; }} class="mt-1 rounded border border-gray-400 px-2 py-1 disabled:opacity-50">{t('reportCatalog.select')}</button>
          </li>
        {/each}
      </ul>
    {/if}
  </div>
  <form onsubmit={calibrate} class="space-y-2 border-t border-gray-200 pt-3 dark:border-gray-700">
    <label class="block">{t('decisionReport.savedId')}<input bind:value={reportId} disabled={busy || disabled} autocomplete="off" class="mt-1 block w-full rounded border border-gray-400 bg-transparent p-2 font-mono text-xs" /></label>
    <p class="text-sm">{t('decisionReport.savedNote')}</p>
    <button disabled={busy || disabled} class="rounded-lg bg-sky-600 px-3 py-2 text-white disabled:opacity-50">{t('manage.planChange')}</button>
  </form>
  {#if error}<p role="alert" class="text-red-700 dark:text-red-300">{error}</p>{/if}
</section>
