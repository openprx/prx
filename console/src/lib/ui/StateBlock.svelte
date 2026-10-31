<script>
  import { t } from '../i18n';

  const VARIANTS = new Set([
    'loading',
    'empty',
    'error',
    'denied',
    'unavailable',
    'disconnected-stale'
  ]);

  let { variant, message = '' } = $props();

  const safeVariant = $derived(VARIANTS.has(variant) ? variant : 'error');
  const defaults = $derived({
    loading: t('common.loading'),
    empty: t('state.empty'),
    error: t('state.error'),
    denied: t('state.denied'),
    unavailable: t('state.unavailable'),
    'disconnected-stale': t('state.disconnected')
  });
  const text = $derived.by(() => {
    if (typeof message === 'string' && message.trim().length > 0) {
      return message;
    }
    const fallback = defaults[safeVariant];
    return typeof fallback === 'string' && fallback.trim().length > 0 ? fallback : t('common.unknown');
  });
  const role = $derived(safeVariant === 'error' || safeVariant === 'denied' ? 'alert' : 'status');
</script>

<div class={`ui-state ui-state-${safeVariant}`} {role}>
  <p>{text}</p>
</div>
