<script>
  import { onMount, onDestroy } from 'svelte';
  import { api } from '../lib/api';
  import DecisionCredential from '../components/DecisionCredential.svelte';
  import ErrorNotice from '../lib/ErrorNotice.svelte';
  import { t } from '../lib/i18n';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import StateBlock from '../lib/ui/StateBlock.svelte';

  // The page shows the last 20 analyses, the API's default read.
  const ANALYSIS_LIMIT = 20;

  let status = $state(null);
  let backend = $state(null);
  let analyses = $state([]);
  let loading = $state(false);
  let error = $state(null);
  let alive = true;
  onDestroy(() => { alive = false; });
  onMount(() => { load(); });

  async function load() {
    if (!alive) return;
    loading = true;
    try {
      const [nextStatus, backends, rows] = await Promise.all([
        api.getDecisionEngineStatus(),
        api.getDecisionEngineBackends(),
        api.getDecisionEngineAnalyses({ limit: ANALYSIS_LIMIT })
      ]);
      if (!alive) return;
      status = nextStatus;
      // The effective backend is the first row; the builtin follows it.
      backend = Array.isArray(backends) && backends.length > 0 ? backends[0] : null;
      analyses = Array.isArray(rows) ? rows.slice(0, ANALYSIS_LIMIT) : [];
      error = null;
    } catch (failure) { if (alive) error = failure; }
    finally { if (alive) loading = false; }
  }

  function created(ms) {
    return Number.isFinite(ms) ? new Date(ms).toISOString() : '—';
  }
</script>

<section class="space-y-5">
  <header class="flex flex-wrap items-center justify-between gap-3">
    <PageHeader title={t('intentAnalysis.title')} description={t('intentAnalysis.description')} />
    <button disabled={loading} onclick={load} class="rounded-lg border border-gray-400 px-3 py-2 disabled:opacity-50">{t('intentAnalysis.refresh')}</button>
  </header>
  {#if loading && !status}
    <StateBlock variant="loading" />
  {:else if error && !status}
    <StateBlock variant="error" message={error instanceof Error ? error.message : t('state.error')} />
  {:else}
    <ErrorNotice {error} />
  {/if}
  {#if status}
    <div class="space-y-2 rounded-xl border border-gray-200 p-4 dark:border-gray-700">
      {#if !status.enabled}<p role="status">{t('intentAnalysis.disabled')}</p>{/if}
      <p>{t('intentAnalysis.mode')}: {status.mode} · {t('intentAnalysis.telemetryCap')}: {status.telemetry_cap}</p>
      <p class="text-sm">{t('intentAnalysis.queue')}: {status.queue_depth} · {t('intentAnalysis.dropped')}: {status.dropped} · {t('intentAnalysis.storeErrors')}: {status.store_errors}</p>
      {#if backend}
        <h3 class="font-semibold">{t('intentAnalysis.backend')}</h3>
        <p class="break-all text-sm">{backend.kind} / {backend.locality} / {backend.model ?? '—'} · {t('intentAnalysis.credential')}: {String(backend.credential_configured)}</p>
      {/if}
    </div>
    <div class="space-y-3 rounded-xl border border-gray-200 p-4 dark:border-gray-700">
      <h3 class="font-semibold">{t('intentAnalysis.analyses')}</h3>
      {#if analyses.length === 0}
        <p class="text-sm">{t('intentAnalysis.noAnalyses')}</p>
      {:else}
        <ul class="space-y-2">
          {#each analyses as row (row.decision_id)}
            <li class="rounded-lg border border-gray-200 p-3 text-sm dark:border-gray-700">
              <p class="break-all font-mono text-xs">{row.decision_id} · {created(row.created_at_ms)}</p>
              <p class="break-words">{t('intentAnalysis.brief')}: {row.brief_redacted}</p>
              <p>{t('intentAnalysis.candidates')}: {Array.isArray(row.candidates) ? row.candidates.length : 0}</p>
            </li>
          {/each}
        </ul>
      {/if}
    </div>
  {/if}
  <DecisionCredential disabled={loading} onstored={load} />
</section>
