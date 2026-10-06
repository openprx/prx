<script>
  import { onMount, onDestroy } from 'svelte';
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';

  let from = $state(new Date(Date.now() - 86400000).toISOString().slice(0, 16));
  let to = $state(new Date().toISOString().slice(0, 16));
  let id = $state('');
  let rows = $state([]);
  let loadedWindow = $state(null);
  let selected = $state(null);
  let busy = $state(false);
  let error = $state('');
  let alive = true;
  onDestroy(() => { alive = false; });
  onMount(() => { load(); });

  // Explicit DTO projection: future server fields never become a raw JSON view.
  function project(value) {
    if (!value || typeof value.decision_id !== 'string') throw new Error(t('decisionInspect.invalidResponse'));
    const candidate = (v) => ({ id: v.id, action: v.action, target: v.target });
    return {
      decision_id: value.decision_id, created_at_ms: value.created_at_ms,
      directive_kind: value.directive_kind, backend_kind: value.backend_kind,
      backend_locality: value.backend_locality, configured_model: value.configured_model,
      policy_version: value.policy_version, latency_ms: value.latency_ms,
      legacy_v1: value.legacy_v1, cache_hit: value.cache_hit,
      outcome_join: value.outcome_join, error_class: value.error_class,
      recorded_projection: value.recorded_projection ? {
        state: value.recorded_projection.state,
        display_redacted: value.recorded_projection.display_redacted === true
      } : null,
      gate_summary: value.gate_summary ? ['pre_prediction','post_rehearsal','pre_commit'].map(phase => ({
        phase, passed:value.gate_summary[phase]?.passed,
        approval_only:value.gate_summary[phase]?.approval_only,
        approval_deferred:value.gate_summary[phase]?.approval_deferred,
        blocked:Object.entries(value.gate_summary[phase]?.blocked || {}).filter(([gate]) => /^[1-9]$/.test(gate))
      })) : null,
      distributions: {
        next_action: value.distributions?.next_action ?? null,
        capability_target: value.distributions?.capability_target ?? null,
        features: value.distributions?.features ?? null
      },
      selected: value.selected ? candidate(value.selected) : null,
      candidates: Array.isArray(value.candidates) ? value.candidates.map(candidate) : [],
      child_outcomes_truncated: value.child_outcomes_truncated === true,
      child_outcomes: Array.isArray(value.child_outcomes) ? value.child_outcomes.map(v => ({
        child_decision_id: v.child_decision_id, settlement_id: v.settlement_id,
        classification: v.classification, completeness: v.completeness
      })) : [],
      outcomes: Array.isArray(value.outcomes) ? value.outcomes.map(v => ({
        settlement_id: v.settlement_id, classification: v.classification, completeness: v.completeness
      })) : []
    };
  }

  function utc(ms) {
    if (!Number.isSafeInteger(ms)) return '—';
    const value = new Date(ms);
    return Number.isNaN(value.getTime()) ? '—' : value.toISOString();
  }

  async function load(event) {
    event?.preventDefault();
    if (busy) return;
    error = '';
    const start = new Date(`${from}Z`), end = new Date(`${to}Z`);
    if (!Number.isFinite(+start) || !Number.isFinite(+end) || start >= end) {
      error = t('decisionInspect.invalidWindow'); return;
    }
    busy = true;
    rows = [];
    loadedWindow = null;
    selected = null;
    try {
      const query = { from: start.toISOString(), to: end.toISOString() };
      const result = await api.getDecisions(query);
      const projected = (result.decisions || []).map(project);
      if (alive) { rows = projected; loadedWindow = query; }
    } catch (failure) {
      if (alive) error = failure instanceof Error ? failure.message : t('decisionInspect.invalidResponse');
    } finally { if (alive) busy = false; }
  }

  async function inspect(value) {
    if (busy) return;
    error = '';
    selected = null;
    const target = value.trim();
    if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(target)) {
      error = t('decisionApproval.invalidId'); return;
    }
    busy = true;
    try {
      const detail = project(await api.getDecision(target));
      if (detail.decision_id.toLowerCase() !== target.toLowerCase()) throw new Error(t('decisionInspect.invalidResponse'));
      if (alive) { selected = detail; id = detail.decision_id; }
    } catch (failure) {
      if (alive) error = failure instanceof Error ? failure.message : t('decisionInspect.invalidResponse');
    } finally { if (alive) busy = false; }
  }
</script>

<section class="space-y-5">
  <header>
    <h2 class="text-2xl font-semibold">{t('decisionInspect.title')}</h2>
    <p class="mt-1 text-sm text-gray-500 dark:text-gray-400">{t('decisionInspect.scope')}</p>
  </header>
  <form onsubmit={load} class="flex flex-wrap items-end gap-3 rounded-xl border border-gray-200 p-4 dark:border-gray-700">
    <label>{t('decisionInspect.from')}<input type="datetime-local" bind:value={from} disabled={busy} class="mt-1 block rounded border border-gray-400 bg-transparent p-2" /></label>
    <label>{t('decisionInspect.to')}<input type="datetime-local" bind:value={to} disabled={busy} class="mt-1 block rounded border border-gray-400 bg-transparent p-2" /></label>
    <button disabled={busy} class="rounded-lg bg-sky-600 px-4 py-2 text-white disabled:opacity-50">{t('decisionInspect.load')}</button>
  </form>
  <form onsubmit={(event) => { event.preventDefault(); inspect(id); }} class="flex flex-wrap items-end gap-3">
    <label class="min-w-0 flex-1">{t('decisionApproval.id')}<input bind:value={id} disabled={busy} autocomplete="off" spellcheck="false" class="mt-1 block w-full rounded border border-gray-400 bg-transparent p-2 font-mono text-xs" /></label>
    <button disabled={busy} class="rounded-lg border border-gray-400 px-4 py-2 disabled:opacity-50">{t('decisionApproval.inspect')}</button>
  </form>
  {#if error}<p role="alert" class="text-red-700 dark:text-red-300">{error}</p>{/if}
  <p aria-live="polite" class="text-sm text-gray-500 dark:text-gray-400">{busy ? t('common.loading') : t('decisionInspect.limit')}</p>
  {#if selected}
    <article class="space-y-3 rounded-xl border border-sky-300 bg-white p-4 dark:border-sky-800 dark:bg-gray-800">
      <h3 class="break-all font-mono text-sm">{selected.decision_id}</h3>
      <dl class="grid gap-2 text-sm sm:grid-cols-2">
        <div><dt>{t('decisionInspect.created')}</dt><dd>{utc(selected.created_at_ms)}</dd></div>
        <div><dt>{t('decisionInspect.cacheHit')}</dt><dd>{typeof selected.cache_hit === 'boolean' ? String(selected.cache_hit) : '—'}</dd></div>
        <div><dt>{t('decisionInspect.directive')}</dt><dd>{selected.directive_kind}</dd></div>
        <div><dt>{t('decisionInspect.backend')}</dt><dd>{selected.backend_kind} / {selected.backend_locality} / {selected.configured_model}</dd></div>
        <div><dt>{t('decisionInspect.policy')}</dt><dd>{selected.policy_version}</dd></div>
        <div><dt>{t('decisionInspect.latency')}</dt><dd>{selected.latency_ms} ms</dd></div>
        <div><dt>{t('decisionInspect.join')}</dt><dd>{selected.outcome_join}</dd></div>
        <div><dt>{t('decisionInspect.error')}</dt><dd>{selected.error_class || '—'}</dd></div>
      </dl>
      {#if selected.legacy_v1}<p>{t('decisionInspect.legacy')}</p>{/if}
      <h4 class="font-semibold">{t('decisionInspect.candidates')}</h4>
      <ul class="space-y-1 text-sm">
        {#each selected.candidates as candidate}
          <li class="break-all">{candidate.action} / {candidate.target} <span class="font-mono text-xs">{candidate.id}</span>{#if selected.selected?.id === candidate.id} — {t('decisionInspect.selected')}{/if}</li>
        {/each}
      </ul>
      <details>
        <summary class="cursor-pointer font-semibold">{t('decisionInspect.projection')}</summary>
        {#if selected.recorded_projection}
          <p class="mt-2 text-sm">{t('decisionInspect.projectionNote')}</p>
          {#if selected.recorded_projection.display_redacted}<p class="text-sm">{t('decisionInspect.projectionRedacted')}</p>{/if}
          <pre class="mt-2 max-h-80 overflow-auto whitespace-pre-wrap break-all rounded-lg bg-gray-100 p-3 text-xs dark:bg-gray-900">{JSON.stringify(selected.recorded_projection.state, null, 2)}</pre>
        {:else}<p class="mt-2">{t('decisionInspect.noProjection')}</p>{/if}
      </details>
      <h4 class="font-semibold">{t('decisionInspect.gates')}</h4>
      {#if selected.gate_summary}
        {#each selected.gate_summary as summary}
          <h5>{t(`decisionInspect.${summary.phase}`)}</h5>
          <p class="text-sm">{t('decisionInspect.gateCounts', summary)}</p>
          {#each summary.blocked as [gate, count]}<p class="text-sm">{t('decisionInspect.blockedCount', {gate, count})}</p>{/each}
        {/each}
      {:else}<p>{t('decisionReport.unavailable')}</p>{/if}
      <h4 class="font-semibold">{t('decisionInspect.distributions')}</h4>
      <p class="text-sm">{t('decisionInspect.probabilityNote')}</p>
      {#each ['next_action', 'capability_target', 'features'] as group}
        <h5>{t(`decisionReport.${group}`)}</h5>
        {#if selected.distributions[group]}
          <dl class="grid gap-2 text-sm sm:grid-cols-2">
            {#each Object.entries(selected.distributions[group]) as [label, probability]}
              <div class="flex justify-between gap-3"><dt>{label}</dt><dd>{typeof probability === 'number' && Number.isFinite(probability) ? probability : '—'}</dd></div>
            {/each}
          </dl>
        {:else}<p>{t('decisionReport.unavailable')}</p>{/if}
      {/each}
      <h4 class="font-semibold">{t('decisionInspect.outcomes')}</h4>
      {#if selected.outcomes.length === 0}<p>{t('decisionInspect.noOutcome')}</p>{/if}
      {#each selected.outcomes as outcome}
        <p class="break-all text-sm">{outcome.classification} · {outcome.completeness} · <span class="font-mono text-xs">{outcome.settlement_id}</span></p>
      {/each}
      <h4 class="font-semibold">{t('decisionInspect.children')}</h4>
      <p class="text-sm">{t('decisionInspect.childrenNote')}</p>
      {#if selected.child_outcomes.length === 0}<p>{t('decisionInspect.noChildren')}</p>{/if}
      {#each selected.child_outcomes as child}
        <div class="break-all text-sm">
          <button disabled={busy} onclick={() => inspect(child.child_decision_id)} class="font-mono text-xs text-sky-700 underline dark:text-sky-300">{child.child_decision_id}</button>
          <span> · {child.classification} · {child.completeness} · {child.settlement_id}</span>
        </div>
      {/each}
      {#if selected.child_outcomes_truncated}<p>{t('decisionInspect.childrenTruncated')}</p>{/if}
    </article>
  {/if}
  {#if loadedWindow}<p class="break-all text-sm">{t('decisionInspect.loadedWindow', loadedWindow)}</p>{/if}
  <div class="overflow-x-auto rounded-xl border border-gray-200 dark:border-gray-700">
    <table class="w-full text-left text-sm">
      <thead><tr><th class="p-3">{t('decisionApproval.id')}</th><th class="p-3">{t('decisionInspect.directive')}</th><th class="p-3">{t('decisionInspect.join')}</th></tr></thead>
      <tbody>
        {#each rows as row}
          <tr class="border-t border-gray-200 dark:border-gray-700"><td class="p-3"><button disabled={busy} onclick={() => inspect(row.decision_id)} class="break-all text-left font-mono text-xs text-sky-700 underline dark:text-sky-300">{row.decision_id}</button></td><td class="p-3">{row.directive_kind}</td><td class="p-3">{row.outcome_join}</td></tr>
        {/each}
      </tbody>
    </table>
  </div>
</section>
