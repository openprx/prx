<script>
  import { t } from '../i18n';

  const TONES = new Set(['neutral', 'success', 'warning', 'danger', 'info']);

  let { label = '', tone = 'neutral' } = $props();

  const safeTone = $derived(TONES.has(tone) ? tone : 'neutral');
  const text = $derived(typeof label === 'string' && label.trim().length > 0 ? label : t('common.unknown'));
</script>

<span class={`ui-badge ui-badge-${safeTone}`}>{text}</span>
