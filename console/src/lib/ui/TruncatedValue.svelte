<script>
  import { t } from '../i18n';

  let { value } = $props();
  let node = $state(null);
  let overflows = $state(false);

  const text = $derived(
    typeof value === 'string' && value.trim().length > 0 ? value : t('common.unknown')
  );

  $effect(() => {
    const current = node;
    const label = text;
    if (!current || label.length === 0) {
      overflows = false;
      return undefined;
    }

    const measure = () => {
      overflows = current.scrollWidth > current.clientWidth;
    };
    measure();

    if (typeof ResizeObserver !== 'function') {
      return undefined;
    }

    const observer = new ResizeObserver(measure);
    observer.observe(current);
    return () => observer.disconnect();
  });
</script>

<!-- Focusable only when the text is clipped, so short values are not tab stops. -->
<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
<span bind:this={node} class="ui-truncate-id" title={text} tabindex={overflows ? 0 : undefined}>{text}</span>
