<script>
  import { onDestroy } from 'svelte';
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';
  let { disabled = false } = $props();
  let busy = $state(false);
  let confirmation = $state(null);
  let result = $state(null);
  let error = $state('');
  let alive = true;
  onDestroy(() => { alive = false; });

  async function run(backend, live = false, confirmed = false) {
    if (busy || disabled || (confirmed && !confirmation)) return;
    const revision = confirmed ? confirmation.revision : null;
    busy = true;
    result = null;
    error = '';
    confirmation = null;
    try {
      const response = await api.testDecisionBackend({ backend, live, confirmHighRisk: confirmed, revision });
      if (!alive) return;
      result = {
        outcome: response.outcome, sent: response.sent, served_by: response.served_by,
        answered: Array.isArray(response.answered)
          ? ['choice', 'score', 'noul'].filter(primitive => response.answered.includes(primitive)) : [],
        attempts: Array.isArray(response.attempts) ? response.attempts.map(a => ({
          backend: a.backend, outcome: a.outcome, error_class: a.error_class, latency_ms: a.latency_ms
        })) : []
      };
    } catch (failure) {
      if (!alive) return;
      if (live && !confirmed && failure?.status === 428 && failure.body?.required === 'confirm_high_risk' && typeof failure.body.revision === 'string') {
        confirmation = { revision: failure.body.revision, target: typeof failure.body.target === 'string' ? failure.body.target : '—' };
      } else {
        // Never retry an uncertain request: it may already have been billed.
        error = t('decisionTest.failed');
      }
    } finally { if (alive) busy = false; }
  }
</script>

<section class="space-y-3 rounded-xl border border-gray-200 p-4 dark:border-gray-700" aria-label={t('decisionTest.title')}>
  <h3 class="font-semibold">{t('decisionTest.title')}</h3>
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('decisionTest.localNote')}</p>
  <div class="flex flex-wrap gap-2">
    <button disabled={disabled || busy} onclick={() => run('builtin')} class="rounded-lg border border-gray-400 px-3 py-2 disabled:opacity-50">{t('decisionTest.builtin')}</button>
    <button disabled={disabled || busy} onclick={() => run('configured')} class="rounded-lg border border-gray-400 px-3 py-2 disabled:opacity-50">{t('decisionTest.local')}</button>
    <button disabled={disabled || busy} onclick={() => run('configured', true)} class="rounded-lg border border-amber-600 px-3 py-2 disabled:opacity-50">{t('decisionTest.prepareLive')}</button>
  </div>
  {#if confirmation}
    <div class="space-y-2 rounded-lg border border-amber-600 p-3">
      <p>{t('decisionTest.liveNote')}</p>
      <p class="break-all font-mono text-xs">{confirmation.target}</p>
      <button disabled={disabled || busy} onclick={() => run('configured', true, true)} class="rounded-lg bg-amber-700 px-3 py-2 text-white disabled:opacity-50">{t('decisionTest.confirm')}</button>
      <button disabled={busy} onclick={() => { confirmation = null; }} class="ml-2 rounded-lg border border-gray-400 px-3 py-2">{t('decisionTest.cancel')}</button>
    </div>
  {/if}
  <div aria-live="polite">
    {#if busy}<p>{t('common.loading')}</p>{/if}
    {#if result}
      <p>{t('decisionTest.outcome')}: {result.outcome}</p>
      <p>{t('decisionTest.sent')}: {String(result.sent)} · {t('decisionTest.served')}: {result.served_by || '—'}</p>
      <p>{t('decisionTest.answered')}: {result.answered.length ? result.answered.join(', ') : t('decisionReport.unavailable')}</p>
      <p class="text-sm">{t('decisionTest.interpretation')}</p>
      {#each result.attempts as attempt}
        <p class="text-sm">{attempt.backend}: {attempt.outcome} · {attempt.latency_ms} ms · {attempt.error_class || '—'}</p>
      {/each}
    {/if}
  </div>
  {#if error}<p role="alert" class="text-red-700 dark:text-red-300">{error}</p>{/if}
</section>
