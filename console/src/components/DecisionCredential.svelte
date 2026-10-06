<script>
  import { onDestroy } from 'svelte';
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';
  import SecretInput from './SecretInput.svelte';
  let { disabled = false, onstored = () => {} } = $props();
  let value = $state('');
  let revision = $state(null);
  let busy = $state(false);
  let error = $state('');
  let notice = $state('');
  let alive = true;
  onDestroy(() => { alive = false; value = ''; revision = null; });

  async function prepare() {
    if (busy || disabled) return;
    busy = true;
    value = '';
    revision = null;
    notice = '';
    error = '';
    try {
      // Read the management ETag before accepting a new secret. The file
      // contents are not retained by this component or sent back in a write.
      const snapshot = await api.getConfigFilesWithRevision();
      if (!alive) return;
      if (typeof snapshot.revision !== 'string' || !/^[0-9a-f]+$/i.test(snapshot.revision)) throw new Error();
      revision = snapshot.revision;
    } catch { if (alive) error = t('decisionCredential.prepareFailed'); }
    finally { if (alive) busy = false; }
  }

  async function save(event) {
    event.preventDefault();
    if (busy || disabled || !revision || !value.trim()) return;
    busy = true;
    error = '';
    notice = '';
    let secret = value;
    const expected = revision;
    value = '';
    revision = null;
    try {
      const receipt = await api.storeDecisionCredential(secret, expected);
      if (!alive) return;
      if (!receipt || typeof receipt.id !== 'string' || !receipt.id.startsWith('cred-')) throw new Error();
      notice = t(receipt.previous_left ? 'decisionCredential.previousLeft' : 'decisionCredential.stored');
      try { await onstored(); }
      catch { if (alive) error = t('decisionCredential.refreshFailed'); }
    } catch {
      // Do not render an error body that could echo user-supplied material.
      if (alive) error = t('decisionCredential.failed');
    } finally {
      secret = '';
      if (alive) busy = false;
    }
  }
</script>

<section class="space-y-3 rounded-xl border border-gray-200 p-4 dark:border-gray-700">
  <h3 class="font-semibold">{t('decisionCredential.title')}</h3>
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('decisionCredential.note')}</p>
  {#if revision}
    <form onsubmit={save} class="space-y-3">
      <SecretInput bind:value label={t('decisionCredential.value')} disabled={busy || disabled} />
      <button disabled={busy || disabled || !value.trim()} class="rounded-lg bg-amber-700 px-3 py-2 text-white disabled:opacity-50">{t('decisionCredential.save')}</button>
      <button type="button" disabled={busy} onclick={() => { value = ''; revision = null; }} class="ml-2 rounded-lg border border-gray-400 px-3 py-2">{t('decisionTest.cancel')}</button>
    </form>
  {:else}
    <button disabled={busy || disabled} onclick={prepare} class="rounded-lg border border-gray-400 px-3 py-2 disabled:opacity-50">{t('decisionCredential.prepare')}</button>
  {/if}
  <p aria-live="polite">{busy ? t('common.loading') : notice}</p>
  {#if error}<p role="alert" class="text-red-700 dark:text-red-300">{error}</p>{/if}
</section>
