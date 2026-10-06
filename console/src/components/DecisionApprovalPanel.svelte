<script>
  import { onDestroy } from 'svelte';
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';

  let { sessionId, onsettled = () => {} } = $props();
  let decisionId = $state('');
  let reviewedId = $state('');
  let view = $state(null);
  let busy = $state(false);
  let error = $state('');
  let notice = $state('');
  let resolved = $state(false);
  let approvalExpires = $state('');
  let alive = true;
  onDestroy(() => { alive = false; });

  async function inspect(event) {
    event.preventDefault();
    if (busy) return;
    const id = decisionId.trim();
    view = null;
    notice = '';
    error = '';
    resolved = false;
    approvalExpires = '';
    reviewedId = '';
    if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(id)) {
      error = t('decisionApproval.invalidId');
      return;
    }
    busy = true;
    try {
      const result = await api.getDecisionApproval(sessionId, id);
      if (!alive) return;
      if (result?.decision_id?.toLowerCase() !== id.toLowerCase()) throw new Error(t('decisionApproval.unavailable'));
      reviewedId = result.decision_id;
      view = {
        tool: result.tool, effect: result.effect, call_digest: result.call_digest,
        pending_expires_at: result.pending_expires_at, arguments: result.arguments,
        arguments_redacted: result.arguments_redacted === true,
        original_call_available: result.original_call_available === true,
        single_use: result.single_use === true
      };
    } catch (failure) {
      if (alive) error = failure instanceof Error ? failure.message : t('decisionApproval.unavailable');
    } finally {
      if (alive) busy = false;
    }
  }

  async function act(action) {
    if (busy || !view || resolved || !['approve', 'reject', 'revoke', 'resume'].includes(action)) return;
    busy = true;
    error = '';
    notice = '';
    try {
      if (action === 'resume') {
        if (!view.original_call_available) return;
        // Send only the reviewed identifier. Display redaction is never call input.
        const result = await api.resumeDecisionApproval(sessionId, reviewedId);
        if (!alive) return;
        resolved = true;
        notice = result?.status === 'ok' && result?.execution_status === 'succeeded'
          ? t('decisionApproval.completed') : t('decisionApproval.notCompleted');
        try { await onsettled(); }
        catch { if (alive) error = t('decisionApproval.refreshFailed'); }
      } else if (action === 'revoke') {
        const receipt = await api.revokeDecisionApproval(sessionId, reviewedId);
        if (!alive) return;
        if (receipt?.decision_id?.toLowerCase() !== reviewedId.toLowerCase()
          || !Number.isSafeInteger(receipt.revoked_grants) || receipt.revoked_grants < 0) throw new Error(t('decisionApproval.unavailable'));
        resolved = true;
        approvalExpires = '';
        notice = t('decisionApproval.revoked');
      } else {
        const approved = action === 'approve';
        const receipt = await api.reviewDecisionApproval(sessionId, reviewedId, approved);
        if (!alive) return;
        if (receipt?.decision_id?.toLowerCase() !== reviewedId.toLowerCase()
          || receipt.approved !== approved || !Number.isSafeInteger(receipt.grant_count)
          || receipt.grant_count < 0 || !Number.isFinite(Date.parse(receipt.expires_at))) {
          throw new Error(t('decisionApproval.unavailable'));
        }
        approvalExpires = approved ? receipt.expires_at : '';
        resolved = !approved;
        notice = t(approved ? 'decisionApproval.approved' : 'decisionApproval.rejected');
      }
    } catch (failure) {
      if (!alive) return;
      // A transport failure cannot establish whether a one-shot call ran.
      // Require a fresh inspection; never retry a mutation automatically.
      resolved = true;
      error = action === 'resume' ? t('decisionApproval.uncertain')
        : failure instanceof Error ? failure.message : t('decisionApproval.unavailable');
    } finally {
      if (alive) busy = false;
    }
  }
</script>

<details class="shrink-0 rounded-xl border border-amber-300 bg-amber-50 p-3 text-sm dark:border-amber-800 dark:bg-gray-900">
  <summary class="cursor-pointer font-semibold text-gray-900 dark:text-gray-100">{t('decisionApproval.title')}</summary>
  <form onsubmit={inspect} class="mt-3 flex flex-wrap items-end gap-2">
    <label class="min-w-0 flex-1 text-gray-700 dark:text-gray-200">
      <span class="mb-1 block">{t('decisionApproval.id')}</span>
      <input bind:value={decisionId} disabled={busy} autocomplete="off" spellcheck="false"
        class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 font-mono text-xs dark:border-gray-600 dark:bg-gray-800" />
    </label>
    <button disabled={busy} class="rounded-lg bg-sky-600 px-3 py-2 text-white disabled:opacity-50">{t('decisionApproval.inspect')}</button>
  </form>
  {#if view}
    <div class="mt-3 max-h-64 space-y-2 overflow-auto text-gray-700 dark:text-gray-200">
      <p class="break-all font-mono text-xs">{reviewedId}</p>
      <p>{t('decisionApproval.tool')}: {view.tool || '—'} · {t('decisionApproval.effect')}: {view.effect}</p>
      <p>{t('decisionApproval.expires')}: {view.pending_expires_at}</p>
      {#if view.single_use}<p>{t('decisionApproval.singleUse')}</p>{/if}
      {#if approvalExpires}<p>{t('decisionApproval.approvalExpires')}: {approvalExpires}</p>{/if}
      <p class="break-all font-mono text-xs">{t('decisionApproval.digest')}: {view.call_digest}</p>
      {#if view.arguments !== null}<pre class="whitespace-pre-wrap break-all rounded-lg bg-white p-3 text-xs dark:bg-gray-800">{JSON.stringify(view.arguments, null, 2)}</pre>{/if}
      {#if view.arguments_redacted}<p>{t('decisionApproval.redacted')}</p>{/if}
      {#if !view.original_call_available}<p>{t('decisionApproval.noSnapshot')}</p>{/if}
      <p>{t('decisionApproval.separate')}</p>
      <div class="flex flex-wrap gap-2">
        <button type="button" disabled={busy || resolved} onclick={() => act('approve')} class="rounded-lg bg-sky-600 px-3 py-2 text-white disabled:opacity-50">{t('decisionApproval.approve')}</button>
        <button type="button" disabled={busy || resolved} onclick={() => act('reject')} class="rounded-lg border border-gray-400 px-3 py-2 disabled:opacity-50">{t('decisionApproval.reject')}</button>
        <button type="button" disabled={busy || resolved} onclick={() => act('revoke')} class="rounded-lg border border-gray-400 px-3 py-2 disabled:opacity-50">{t('decisionApproval.revoke')}</button>
        <button type="button" disabled={busy || resolved || !view.original_call_available} onclick={() => act('resume')} class="rounded-lg bg-amber-700 px-3 py-2 text-white disabled:opacity-50">{t('decisionApproval.resume')}</button>
      </div>
    </div>
  {/if}
  <div aria-live="polite" class="mt-2 text-gray-700 dark:text-gray-200">{busy ? t('common.loading') : notice}</div>
  {#if error}<p role="alert" class="mt-2 text-red-700 dark:text-red-300">{error}</p>{/if}
</details>
