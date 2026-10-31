<script>
  import { t } from '../i18n';

  let { value } = $props();
  const text = $derived(
    typeof value === 'string' && value.trim().length > 0 ? value : t('common.unknown')
  );
</script>

<button type="button" class="ui-truncate-id">{text}</button>
