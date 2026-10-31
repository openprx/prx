<script>
  import { t } from '../lib/i18n';
  import { createConversation, refreshConversationList, sessionCreate } from '../lib/new-conversation.svelte.js';

  let { placement = 'header' } = $props();

  const showsCreateState = $derived(placement === 'header');
  const describedBy = $derived(
    showsCreateState && sessionCreate.uncertain ? `new-conversation-uncertain-${placement}` : undefined
  );
</script>

<div class="flex min-w-0 max-w-full flex-wrap items-center gap-2">
  <button
    type="button"
    class={`ui-new-conversation ui-new-conversation-${placement} rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:cursor-not-allowed disabled:opacity-50`}
    disabled={sessionCreate.pending}
    aria-busy={sessionCreate.pending}
    aria-describedby={describedBy}
    onclick={() => createConversation()}
  >
    {sessionCreate.pending ? t('sessions.creating') : t('shell.newConversation')}
  </button>
  {#if showsCreateState && sessionCreate.error}
    <p role="alert" class="max-w-full text-sm text-red-700 dark:text-red-300">{sessionCreate.error}</p>
  {/if}
  {#if showsCreateState && sessionCreate.uncertain}
    <p id={`new-conversation-uncertain-${placement}`} role="status" class="max-w-full text-sm text-amber-800 dark:text-amber-200">
      {t('sessions.uncertain')}
    </p>
    <button
      type="button"
      class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200"
      onclick={refreshConversationList}
    >
      {t('sessions.refreshList')}
    </button>
  {/if}
</div>
