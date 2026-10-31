<script>
  import { t } from '../lib/i18n';
  import StateBlock from '../lib/ui/StateBlock.svelte';
  import StatusBadge from '../lib/ui/StatusBadge.svelte';

  let {
    phase = 'accepted',
    elapsedMs = 0,
    jobId = '',
    workId = '',
    stillRunning = false,
    pollStale = false,
    ack = '',
    uncertain = false,
    onCancel,
    onCheckAgain,
    jobMissing = false
  } = $props();

  const tones = {
    accepted: 'info',
    running: 'info',
    succeeded: 'success',
    failed: 'danger',
    cancelled: 'warning'
  };
  const phaseKey = $derived(`chat.${phase}`);
  const phaseLabel = $derived.by(() => {
    const translated = t(phaseKey);
    return translated === phaseKey ? phase : translated;
  });
  const elapsed = $derived(Number.isFinite(Number(elapsedMs)) ? String(elapsedMs) : '0');
  const active = $derived(phase === 'accepted' || phase === 'running');
</script>

<section class="space-y-2 rounded-xl border border-sky-200 bg-sky-50 p-3 text-sm dark:border-sky-900 dark:bg-gray-900" aria-live="polite">
  <div class="flex flex-wrap items-center gap-2">
    <span>{t('chat.phase')}</span>
    <StatusBadge label={phaseLabel} tone={tones[phase] || 'neutral'} />
    <span>{t('chat.elapsed', { ms: elapsed })}</span>
  </div>
  <p class="break-all font-mono text-xs text-gray-600 dark:text-gray-300">
    {t('chat.job')}: {jobId || t('common.unknown')} · {t('chat.work')}: {workId || t('common.unknown')}
  </p>
  {#if pollStale}
    <StateBlock variant="disconnected-stale" message={t('chat.pollStale')} />
  {/if}
  {#if uncertain}
    <p role="status">{t('chat.uncertainSend')}</p>
  {/if}
  {#if stillRunning}
    <p role="status">{t('chat.stillRunning')}</p>
    <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm dark:border-gray-600 dark:bg-gray-800" onclick={() => onCheckAgain?.()}>
      {t('chat.checkAgain')}
    </button>
  {/if}
  {#if ack}
    <p role="status" class="break-all">{ack}</p>
  {/if}
  {#if jobMissing}
    <StateBlock variant="error" message={t('chat.jobUnknown')} />
  {/if}
  {#if active && onCancel && !jobMissing}
    <button
      type="button"
      class="rounded-lg border border-amber-700 px-3 py-2 text-sm text-amber-900 dark:text-amber-200"
      onclick={() => onCancel()}
    >
      {t('chat.cancelTurn')}
    </button>
  {/if}
</section>
