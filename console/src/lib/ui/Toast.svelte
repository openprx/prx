<script>
  import { t } from '../i18n';

  let { message = '', onDismiss } = $props();
</script>

<div class="ui-toast" role="status" aria-live="polite" aria-atomic="true" aria-label={t('toast.region')}>
  {#if message}
    <p>{message}</p>
  {/if}
  {#if onDismiss}
    <button type="button" onclick={() => onDismiss()}>{t('toast.dismiss')}</button>
  {/if}
</div>
