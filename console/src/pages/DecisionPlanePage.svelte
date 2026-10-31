<script>
  import { onMount, onDestroy } from 'svelte';
  import { api } from '../lib/api';
  import { ChangeFlow } from '../lib/change-flow.svelte.js';
  import DecisionReport from '../components/DecisionReport.svelte';
  import DecisionCredential from '../components/DecisionCredential.svelte';
  import DecisionBackendTest from '../components/DecisionBackendTest.svelte';
  import PlanReview from '../lib/PlanReview.svelte';
  import ErrorNotice from '../lib/ErrorNotice.svelte';
  import { t } from '../lib/i18n';
  import PageHeader from '../lib/ui/PageHeader.svelte';

  let status = $state(null);
  let loading = $state(false);
  let error = $state(null);
  let kind = $state('builtin');
  let endpoint = $state('');
  let model = $state('');
  let snapshot = $state('');
  let timeout = $state('');
  let credentialRef = $state('');
  let policy = $state({ max_candidates: '', max_decision_hops: '', extra_latency_budget_ms: '', commit_threshold: '', minimum_choice_confidence: '', minimum_top_margin: '', extra_token_ratio_limit: '' });
  let alive = true;
  const flow = new ChangeFlow(load, { plan: api.planDecisionChange, apply: api.applyDecisionChange });
  const busy = $derived(loading || flow.busy);
  onDestroy(() => { alive = false; flow.dispose(); });
  onMount(() => { load(); });

  async function load() {
    if (!alive) return;
    loading = true;
    try {
      const result = await api.getDecisionStatus();
      if (alive) { status = result; error = null; }
    } catch (failure) { if (alive) error = failure; }
    finally { if (alive) loading = false; }
  }

  function preview(mutation) {
    if (!busy) flow.preview([mutation]);
  }

  function backend(event) {
    event.preventDefault();
    const mutation = { op: 'set_backend', kind };
    if (endpoint.trim()) mutation.endpoint = endpoint.trim();
    if (model.trim()) mutation.model = model.trim();
    if (snapshot.trim()) mutation.pinned_snapshot = snapshot.trim();
    if (timeout !== undefined && timeout !== null && String(timeout).trim() !== '') {
      const value = Number(timeout);
      if (!Number.isInteger(value) || value < 10 || value > 4294967295) {
        error = new Error(t('decisionPlane.invalidTimeout')); return;
      }
      mutation.timeout_ms = value;
    }
    preview(mutation);
  }

  function policyPlan(event) {
    event.preventDefault();
    const mutation = { op: 'set_policy' };
    for (const [key, value] of Object.entries(policy)) {
      if (value === undefined || value === null || String(value).trim() === '') continue;
      const number = Number(value);
      if (!Number.isFinite(number)) { error = new Error(t('decisionPlane.invalidNumber')); return; }
      mutation[key] = number;
    }
    if (Object.keys(mutation).length > 1) preview(mutation);
  }
</script>

<section class="space-y-5">
  <header class="flex flex-wrap items-center justify-between gap-3">
    <PageHeader title={t('decisionPlane.title')} />
    <button disabled={busy} onclick={load} class="rounded-lg border border-gray-400 px-3 py-2 disabled:opacity-50">{t('decisionPlane.refresh')}</button>
  </header>
  <ErrorNotice {error} />
  {#if status}
    <div class="space-y-3 rounded-xl border border-gray-200 p-4 dark:border-gray-700">
      <p>{t('decisionPlane.enabled')}: {String(status.enabled)} · {t('decisionPlane.key')}: {String(status.key_available)}</p>
      <p>{t('decisionPlane.remote')}: {String(status.remote_enabled)} · {t('decisionPlane.timeout')}: {status.backend_timeout_ms ?? '—'}</p>
      <details><summary class="cursor-pointer">{t('decisionPlane.currentPolicy')}</summary>
        <dl class="mt-2 grid gap-2 text-sm sm:grid-cols-2">{#each Object.keys(policy) as key}<div><dt>{t(`decisionPlane.${key}`)}</dt><dd>{status.policy?.[key] ?? '—'}</dd></div>{/each}</dl>
      </details>
      {#if status.factory_error}<p role="alert" class="text-red-700 dark:text-red-300">{status.factory_error}</p>{/if}
      <h3 class="font-semibold">{t('decisionPlane.backends')}</h3>
      {#each status.order || [] as item}
        <p class="break-all text-sm">{item.kind} / {item.locality} / {item.model} — {item.health} · {item.circuit} {item.last_failure || ''}</p>
      {/each}
      {#each status.excluded || [] as item}<p class="text-sm">{item.kind}: {item.reason}</p>{/each}
      <p class="text-sm">{t('decisionPlane.store')}: {status.store?.mode} · {status.store?.decisions ?? '—'} / {status.store?.outcomes ?? '—'}</p>
      {#if status.store?.error}<p role="alert">{status.store.error}</p>{/if}
      <button disabled={busy} onclick={() => preview({ op: 'set_enabled', enabled: !status.enabled })} class="rounded-lg bg-sky-600 px-3 py-2 text-white disabled:opacity-50">{t(status.enabled ? 'decisionPlane.disable' : 'decisionPlane.enable')}</button>
    </div>
  {/if}
  <DecisionCredential disabled={busy} onstored={load} />
  <DecisionBackendTest disabled={busy} />
  <DecisionReport disabled={busy} oncalibrate={(reportId) => preview({ op: 'apply_calibration', report_id: reportId })} />
  <PlanReview {flow} />
  <p class="text-sm text-gray-500 dark:text-gray-400">{t('decisionPlane.previewOnly')}</p>
  <fieldset disabled={busy} class="space-y-3 rounded-xl border border-gray-200 p-4 disabled:opacity-50 dark:border-gray-700">
    <legend class="px-1 font-semibold">{t('decisionPlane.remote')}</legend>
    <p class="text-sm">{t('decisionPlane.remoteNote')}</p>
    <div class="flex flex-wrap gap-2">
      <button onclick={() => preview({ op: 'set_remote', remote: true })} class="rounded-lg border border-amber-600 px-3 py-2">{t('decisionPlane.remoteEnable')}</button>
      <button onclick={() => preview({ op: 'set_remote', remote: false })} class="rounded-lg border border-gray-400 px-3 py-2">{t('decisionPlane.remoteDisable')}</button>
    </div>
  </fieldset>
  <form onsubmit={backend} class="space-y-3 rounded-xl border border-gray-200 p-4 dark:border-gray-700">
    <h3 class="font-semibold">{t('decisionPlane.backend')}</h3>
    <fieldset disabled={busy} class="grid gap-3 sm:grid-cols-2">
      <label>{t('decisionPlane.kind')}<select bind:value={kind} class="mt-1 block w-full rounded border border-gray-400 bg-transparent p-2">{#each ['builtin','compatible_http','typesafe','openrouter'] as value}<option {value}>{value}</option>{/each}</select></label>
      <label>{t('decisionPlane.endpoint')}<input bind:value={endpoint} autocomplete="off" class="mt-1 block w-full rounded border border-gray-400 bg-transparent p-2" /></label>
      <label>{t('decisionPlane.model')}<input bind:value={model} class="mt-1 block w-full rounded border border-gray-400 bg-transparent p-2" /></label>
      <label>{t('decisionPlane.timeout')}<input type="number" min="10" step="1" bind:value={timeout} class="mt-1 block w-full rounded border border-gray-400 bg-transparent p-2" /></label>
      <label>{t('decisionPlane.snapshot')}<input bind:value={snapshot} class="mt-1 block w-full rounded border border-gray-400 bg-transparent p-2" /></label>
    </fieldset>
    <button disabled={busy} class="rounded-lg bg-sky-600 px-3 py-2 text-white disabled:opacity-50">{t('manage.planChange')}</button>
  </form>
  <form onsubmit={(event) => { event.preventDefault(); preview({ op:'set_credential_ref', reference:credentialRef.trim() }); }} class="space-y-3 rounded-xl border border-gray-200 p-4 dark:border-gray-700">
    <label class="block">{t('decisionPlane.credentialRef')}<input disabled={busy} bind:value={credentialRef} autocomplete="off" class="mt-1 block w-full rounded border border-gray-400 bg-transparent p-2" /></label>
    <p class="text-sm">{t('decisionPlane.referenceOnly')}</p>
    <button disabled={busy} class="rounded-lg bg-sky-600 px-3 py-2 text-white disabled:opacity-50">{t('manage.planChange')}</button>
  </form>
  <form onsubmit={policyPlan} class="space-y-3 rounded-xl border border-gray-200 p-4 dark:border-gray-700">
    <h3 class="font-semibold">{t('decisionPlane.policy')}</h3>
    <fieldset disabled={busy} class="grid gap-3 sm:grid-cols-2">
      {#each Object.keys(policy) as key}
        <label>{t(`decisionPlane.${key}`)}<input type="number" step="any" bind:value={policy[key]} class="mt-1 block w-full rounded border border-gray-400 bg-transparent p-2" /></label>
      {/each}
    </fieldset>
    <button disabled={busy} class="rounded-lg bg-sky-600 px-3 py-2 text-white disabled:opacity-50">{t('manage.planChange')}</button>
    <button type="button" disabled={busy} onclick={() => preview({ op:'reset_policy' })} class="ml-2 rounded-lg border border-gray-400 px-3 py-2 disabled:opacity-50">{t('decisionPlane.reset')}</button>
  </form>
</section>
