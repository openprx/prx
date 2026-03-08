<script>
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';

  const HOOK_EVENTS = [
    'agent_start',
    'agent_end',
    'llm_request',
    'llm_response',
    'tool_call_start',
    'tool_call',
    'turn_complete',
    'error'
  ];

  let hooks = $state([]);
  let hooksEnabled = $state(true);
  let loading = $state(true);
  let errorMessage = $state('');
  let actionError = $state('');
  let editing = $state(null);
  let showAddForm = $state(false);
  let saving = $state(false);
  let deletingId = $state('');
  let togglingId = $state('');
  let addFormPrefix = $state('hook-add');

  let formEvent = $state(HOOK_EVENTS[0]);
  let formCommand = $state('');
  let formTimeout = $state(30000);
  let formEnabled = $state(true);

  function resetForm() {
    formEvent = HOOK_EVENTS[0];
    formCommand = '';
    formTimeout = 30000;
    formEnabled = true;
  }

  function fieldId(prefix, name) {
    return `${prefix}-${name}`;
  }

  function humanizeEvent(event) {
    const key = `hooks.events.${event}`;
    const translated = t(key);
    if (translated !== key) {
      return translated;
    }

    return event
      .split('_')
      .map((word) => word.charAt(0).toUpperCase() + word.slice(1))
      .join(' ');
  }

  function validateForm() {
    if (!formCommand.trim()) {
      actionError = t('hooks.commandRequired');
      return false;
    }

    if (!Number.isFinite(Number(formTimeout)) || Number(formTimeout) < 1000) {
      actionError = t('hooks.timeoutInvalid');
      return false;
    }

    return true;
  }

  async function loadHooks() {
    loading = true;

    try {
      const response = await api.getHooks();
      hooks = Array.isArray(response?.hooks) ? response.hooks : [];
      hooksEnabled = response?.enabled !== false;
      errorMessage = '';
      actionError = '';
    } catch (error) {
      hooks = [];
      hooksEnabled = true;
      errorMessage = error instanceof Error ? error.message : t('hooks.loadFailed');
    } finally {
      loading = false;
    }
  }

  function startEdit(hook) {
    editing = hook.id;
    actionError = '';
    formEvent = hook.event;
    formCommand = hook.command;
    formTimeout = hook.timeout_ms;
    formEnabled = hook.enabled;
  }

  function cancelEdit() {
    editing = null;
    actionError = '';
    resetForm();
  }

  async function saveEdit(hookId) {
    if (!validateForm()) return;

    saving = true;
    actionError = '';

    try {
      await api.updateHook(hookId, {
        event: formEvent,
        command: formCommand.trim(),
        timeout_ms: Number(formTimeout)
      });
      editing = null;
      resetForm();
      await loadHooks();
    } catch (error) {
      actionError = error instanceof Error ? error.message : t('hooks.saveFailed');
    } finally {
      saving = false;
    }
  }

  async function addHook() {
    if (!validateForm()) return;

    saving = true;
    actionError = '';

    try {
      await api.createHook({
        event: formEvent,
        command: formCommand.trim(),
        timeout_ms: Number(formTimeout)
      });
      showAddForm = false;
      resetForm();
      await loadHooks();
    } catch (error) {
      actionError = error instanceof Error ? error.message : t('hooks.saveFailed');
    } finally {
      saving = false;
    }
  }

  async function deleteHook(hookId) {
    deletingId = hookId;
    actionError = '';

    try {
      await api.deleteHook(hookId);
      if (editing === hookId) {
        cancelEdit();
      }
      await loadHooks();
    } catch (error) {
      actionError = error instanceof Error ? error.message : t('hooks.deleteFailed');
    } finally {
      deletingId = '';
    }
  }

  async function toggleHook(hookId) {
    togglingId = hookId;
    actionError = '';

    try {
      await api.toggleHook(hookId);
      await loadHooks();
    } catch (error) {
      actionError = error instanceof Error ? error.message : t('hooks.toggleFailed');
    } finally {
      togglingId = '';
    }
  }

  $effect(() => {
    loadHooks();
  });
</script>

<section class="space-y-6">
  <div class="flex items-center justify-between">
    <h2 class="text-2xl font-semibold">{t('hooks.title')}</h2>
    <button
      type="button"
      onclick={() => {
        showAddForm = !showAddForm;
        actionError = '';
        if (showAddForm) resetForm();
      }}
      class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"
    >
      {showAddForm ? t('hooks.cancelAdd') : t('hooks.addHook')}
    </button>
  </div>

  <div class="flex items-center gap-3 rounded-xl border border-gray-200 bg-white px-4 py-3 dark:border-gray-700 dark:bg-gray-800">
    <span class="text-sm font-medium text-gray-700 dark:text-gray-200">{t('hooks.globalStatus')}</span>
    <button
      type="button"
      onclick={() => toggleHook(hooks[0]?.id ?? '')}
      disabled={hooks.length === 0 || togglingId !== ''}
      aria-label={hooksEnabled ? t('common.disabled') : t('common.enabled')}
      class={`relative inline-flex h-5 w-9 items-center rounded-full transition ${hooksEnabled ? 'bg-sky-600' : 'bg-gray-400 dark:bg-gray-600'}`}
    >
      <span class={`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${hooksEnabled ? 'translate-x-4' : 'translate-x-1'}`}></span>
    </button>
    <span class="text-xs text-gray-500 dark:text-gray-400">{t('hooks.globalToggleHint')}</span>
  </div>

  {#if showAddForm}
    <div class="space-y-3 rounded-xl border border-sky-500/30 bg-white p-4 dark:bg-gray-800">
      <h3 class="text-base font-semibold text-gray-900 dark:text-gray-100">{t('hooks.newHook')}</h3>
      <div class="grid gap-3 sm:grid-cols-2">
        <div>
          <label for={fieldId(addFormPrefix, 'event')} class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('hooks.event')}</label>
          <select id={fieldId(addFormPrefix, 'event')} bind:value={formEvent} class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200">
            {#each HOOK_EVENTS as event}
              <option value={event}>{humanizeEvent(event)}</option>
            {/each}
          </select>
        </div>
        <div>
          <label for={fieldId(addFormPrefix, 'timeout')} class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('hooks.timeout')}</label>
          <input
            id={fieldId(addFormPrefix, 'timeout')}
            type="number"
            bind:value={formTimeout}
            min="1000"
            step="1000"
            class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
          />
        </div>
        <div class="sm:col-span-2">
          <label for={fieldId(addFormPrefix, 'command')} class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('hooks.command')}</label>
          <input
            id={fieldId(addFormPrefix, 'command')}
            type="text"
            bind:value={formCommand}
            placeholder={t('hooks.commandPlaceholder')}
            class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
          />
        </div>
        <div class="flex items-center gap-2">
          <span class="text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('hooks.enabled')}</span>
          <button
            type="button"
            disabled
            aria-label={t('hooks.enabled')}
            class={`relative inline-flex h-5 w-9 items-center rounded-full transition ${formEnabled ? 'bg-sky-600' : 'bg-gray-400 dark:bg-gray-600'}`}
          >
            <span class={`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${formEnabled ? 'translate-x-4' : 'translate-x-1'}`}></span>
          </button>
          <span class="text-xs text-gray-400 dark:text-gray-500">{t('hooks.globalToggleHint')}</span>
        </div>
      </div>

      {#if actionError}
        <p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300">
          {actionError}
        </p>
      {/if}

      <div class="flex justify-end gap-2 pt-2">
        <button
          type="button"
          onclick={() => {
            showAddForm = false;
            actionError = '';
            resetForm();
          }}
          class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
        >
          {t('hooks.cancel')}
        </button>
        <button
          type="button"
          onclick={addHook}
          disabled={saving}
          class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"
        >
          {saving ? t('hooks.saving') : t('hooks.save')}
        </button>
      </div>
    </div>
  {/if}

  {#if loading}
    <p class="text-sm text-gray-500 dark:text-gray-400">{t('hooks.loading')}</p>
  {:else if errorMessage}
    <p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300">
      {errorMessage}
    </p>
  {:else if hooks.length === 0}
    <p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300">
      {t('hooks.noHooks')}
    </p>
  {:else}
    {#if actionError}
      <p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300">
        {actionError}
      </p>
    {/if}

    <div class="space-y-3">
      {#each hooks as hook (hook.id)}
        <article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800">
          {#if editing === hook.id}
            <div class="space-y-3">
              <div class="grid gap-3 sm:grid-cols-2">
                <div>
                  <label for={fieldId(hook.id, 'event')} class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('hooks.event')}</label>
                  <select id={fieldId(hook.id, 'event')} bind:value={formEvent} class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200">
                    {#each HOOK_EVENTS as event}
                      <option value={event}>{humanizeEvent(event)}</option>
                    {/each}
                  </select>
                </div>
                <div>
                  <label for={fieldId(hook.id, 'timeout')} class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('hooks.timeout')}</label>
                  <input
                    id={fieldId(hook.id, 'timeout')}
                    type="number"
                    bind:value={formTimeout}
                    min="1000"
                    step="1000"
                    class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
                  />
                </div>
                <div class="sm:col-span-2">
                  <label for={fieldId(hook.id, 'command')} class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('hooks.command')}</label>
                  <input
                    id={fieldId(hook.id, 'command')}
                    type="text"
                    bind:value={formCommand}
                    class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
                  />
                </div>
                <div class="flex items-center gap-2">
                  <span class="text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400">{t('hooks.enabled')}</span>
                  <button
                    type="button"
                    disabled
                    aria-label={t('hooks.enabled')}
                    class={`relative inline-flex h-5 w-9 items-center rounded-full transition ${formEnabled ? 'bg-sky-600' : 'bg-gray-400 dark:bg-gray-600'}`}
                  >
                    <span class={`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${formEnabled ? 'translate-x-4' : 'translate-x-1'}`}></span>
                  </button>
                  <span class="text-xs text-gray-400 dark:text-gray-500">{t('hooks.globalToggleHint')}</span>
                </div>
              </div>

              {#if actionError}
                <p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300">
                  {actionError}
                </p>
              {/if}

              <div class="flex justify-end gap-2">
                <button
                  type="button"
                  onclick={cancelEdit}
                  class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
                >
                  {t('hooks.cancel')}
                </button>
                <button
                  type="button"
                  onclick={() => saveEdit(hook.id)}
                  disabled={saving}
                  class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"
                >
                  {saving ? t('hooks.saving') : t('hooks.save')}
                </button>
              </div>
            </div>
          {:else}
            <div class="flex items-start justify-between gap-3">
              <div class="min-w-0 flex-1">
                <div class="flex items-center gap-3">
                  <h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100">{humanizeEvent(hook.event)}</h3>
                  <span
                    class={`rounded-full px-2 py-1 text-xs font-medium ${
                      hooksEnabled
                        ? 'border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300'
                        : 'border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300'
                    }`}
                  >
                    {hooksEnabled ? t('common.enabled') : t('common.disabled')}
                  </span>
                </div>
                <p class="mt-2 font-mono text-sm text-gray-500 dark:text-gray-400">{hook.command}</p>
                <p class="mt-1 text-xs text-gray-400 dark:text-gray-500">{t('hooks.timeout')}: {hook.timeout_ms}ms</p>
              </div>
              <div class="flex items-center gap-2">
                <button
                  type="button"
                  onclick={() => startEdit(hook)}
                  class="rounded-lg border border-gray-300 bg-white px-2 py-1 text-xs text-gray-600 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700"
                >
                  {t('hooks.edit')}
                </button>
                <button
                  type="button"
                  onclick={() => deleteHook(hook.id)}
                  disabled={deletingId === hook.id}
                  class="rounded-lg border border-red-500/50 bg-red-500/10 px-2 py-1 text-xs text-red-600 hover:bg-red-500/20 disabled:opacity-50 dark:text-red-300"
                >
                  {deletingId === hook.id ? t('hooks.deleting') : t('hooks.delete')}
                </button>
              </div>
            </div>
          {/if}
        </article>
      {/each}
    </div>
  {/if}
</section>
