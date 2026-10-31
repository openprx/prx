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

    // Focusability follows the collapsed measurement only. The :focus rule
    // expands the text, and measuring that expanded box would clear tabindex
    // from the element that holds focus.
    let focused = false;

    const measureCollapsed = () => {
      if (focused || current === document.activeElement) {
        return;
      }
      overflows = current.scrollWidth > current.clientWidth;
    };

    measureCollapsed();

    const onFocusIn = () => {
      focused = true;
    };
    const onFocusOut = () => {
      focused = false;
      requestAnimationFrame(() => {
        if (!current.isConnected || focused || current === document.activeElement) {
          return;
        }
        overflows = current.scrollWidth > current.clientWidth;
      });
    };

    current.addEventListener('focusin', onFocusIn);
    current.addEventListener('focusout', onFocusOut);

    let observer;
    if (typeof ResizeObserver === 'function') {
      observer = new ResizeObserver(() => {
        measureCollapsed();
      });
      observer.observe(current);
    }

    return () => {
      current.removeEventListener('focusin', onFocusIn);
      current.removeEventListener('focusout', onFocusOut);
      observer?.disconnect();
    };
  });
</script>

<!-- Focusable only when the collapsed text is clipped, so short values are not tab stops. -->
<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
<span bind:this={node} class="ui-truncate-id" title={text} tabindex={overflows ? 0 : undefined}>{text}</span>
