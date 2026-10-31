<script>
  import { untrack } from 'svelte';
  import { t } from '../i18n';

  let {
    open = false,
    title = '',
    objectName = '',
    consequence = '',
    confirmLabel = '',
    cancelLabel = '',
    onConfirm,
    onCancel
  } = $props();

  let dialog = $state(null);

  const heading = $derived(title.trim().length > 0 ? title : t('dialog.title'));
  const objectText = $derived(objectName.trim().length > 0 ? objectName.trim() : t('common.unknown'));
  const consequenceText = $derived(
    consequence.trim().length > 0 ? consequence.trim() : t('common.unknown')
  );
  const confirmText = $derived(confirmLabel.trim().length > 0 ? confirmLabel : t('dialog.confirm'));
  const cancelText = $derived(cancelLabel.trim().length > 0 ? cancelLabel : t('dialog.cancel'));

  function focusable(node) {
    return [...node.querySelectorAll('button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])')].filter(
      (element) => !element.hasAttribute('disabled')
    );
  }

  $effect(() => {
    if (!open || !dialog) {
      return undefined;
    }

    const node = dialog;
    const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const initial = focusable(node)[0] ?? node;
    initial.focus();

    const onKey = (event) => {
      if (event.key === 'Escape') {
        event.preventDefault();
        event.stopPropagation();
        untrack(() => onCancel)?.();
        return;
      }
      if (event.key !== 'Tab') {
        return;
      }
      const items = focusable(node);
      if (items.length === 0) {
        event.preventDefault();
        node.focus();
        return;
      }
      const first = items[0];
      const last = items[items.length - 1];
      const active = document.activeElement;
      if (event.shiftKey && (active === first || active === node)) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && active === last) {
        event.preventDefault();
        first.focus();
      }
    };

    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('keydown', onKey);
      if (previous?.isConnected) {
        previous.focus();
      }
    };
  });
</script>

{#if open}
  <div class="ui-dialog-backdrop">
    <div
      bind:this={dialog}
      class="ui-dialog"
      role="dialog"
      aria-modal="true"
      aria-labelledby="confirm-dialog-title"
      aria-describedby="confirm-dialog-body"
      tabindex="-1"
    >
      <h2 id="confirm-dialog-title" class="text-lg font-semibold">{heading}</h2>
      <p id="confirm-dialog-body" class="ui-dialog-body">
        <span class="ui-dialog-object">{objectText}</span>
        <span class="ui-dialog-consequence">{consequenceText}</span>
      </p>
      <div class="ui-dialog-actions">
        <button type="button" class="ui-dialog-cancel" onclick={() => onCancel?.()}>{cancelText}</button>
        <button type="button" class="ui-dialog-confirm" onclick={() => onConfirm?.()}>{confirmText}</button>
      </div>
    </div>
  </div>
{/if}
