<script>
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';

  const HOOK_EVENTS = [
    'agent_start',
    'agent_end',
    'llm_request',
    'llm_response',
    'tool_call_start',
    'tool_call_end',
    'message_received',
    'message_sent'
  ];

  let hooks = $state([]);
  let loading = $state(true);
  let errorMessage = $state('');
  let editing = $state(null);
  let showAddForm = $state(false);

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

  function humanizeEvent(event) {
    return event
      .split('_')
      .map((w) => w.charAt(0).toUpperCase() + w.slice(1))
      .join(' ');
  }

  async function loadHooks() {
    try {
      const response = await api.getHooks();
      hooks = Array.isArray(response?.hooks) ? response.hooks : [];
      errorMessage = '';
    } catch {
      // API not implemented yet - use mock data
      hooks = [
        { id: '1', event: 'message_received', command: 'echo "msg received"', timeout_ms: 30000, enabled: true },
        { id: '2', event: 'agent_start', command: '/opt/scripts/on-start.sh', timeout_ms: 10000, enabled: true },
        { id: '3', event: 'tool_call_end', command: 'notify-send "tool done"', timeout_ms: 5000, enabled: false }
      ];
      errorMessage = '';
    } finally {
      loading = false;
    }
  }

  function startEdit(hook) {
    editing = hook.id;
    formEvent = hook.event;
    formCommand = hook.command;
    formTimeout = hook.timeout_ms;
    formEnabled = hook.enabled;
  }

  function cancelEdit() {
    editing = null;
    resetForm();
  }

  function saveEdit(hookId) {
    hooks = hooks.map((h) =>
      h.id === hookId
        ? { ...h, event: formEvent, command: formCommand, timeout_ms: formTimeout, enabled: formEnabled }
        : h
    );
    editing = null;
    resetForm();
  }

  function addHook() {
    if (!formCommand.trim()) return;
    const newHook = {
      id: String(Date.now()),
      event: formEvent,
      command: formCommand.trim(),
      timeout_ms: formTimeout,
      enabled: formEnabled
    };
    hooks = [...hooks, newHook];
    showAddForm = false;
    resetForm();
  }

  function deleteHook(hookId) {
    hooks = hooks.filter((h) => h.id !== hookId);
  }

  function toggleHook(hookId) {
    hooks = hooks.map((h) => (h.id === hookId ? { ...h, enabled: !h.enabled } : h));
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
      onclick={() => { showAddForm = !showAddForm; if (showAddForm) resetForm(); }}
      class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"
    >
      {showAddForm ? t('hooks.cancelAdd') : t('hooks.addHook')}
    </button>
  </div>

  {#if showAddForm}
    <div class="rounded-xl border border-sky-500/30 bg-gray-800 p-4 space-y-3">
      <h3 class="text-base font-semibold text-gray-100">{t('hooks.newHook')}</h3>
      <div class="grid gap-3 sm:grid-cols-2">
        <div>
          <label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-400">{t('hooks.event')}</label>
          <select bind:value={formEvent} class="w-full rounded-lg border border-gray-600 bg-gray-900 px-3 py-2 text-sm text-gray-200">
            {#each HOOK_EVENTS as ev}
              <option value={ev}>{humanizeEvent(ev)}</option>
            {/each}
          </select>
        </div>
        <div>
          <label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-400">{t('hooks.timeout')}</label>
          <input type="number" bind:value={formTimeout} min="1000" step="1000"
            class="w-full rounded-lg border border-gray-600 bg-gray-900 px-3 py-2 text-sm text-gray-200" />
        </div>
        <div class="sm:col-span-2">
          <label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-400">{t('hooks.command')}</label>
          <input type="text" bind:value={formCommand} placeholder={t('hooks.commandPlaceholder')}
            class="w-full rounded-lg border border-gray-600 bg-gray-900 px-3 py-2 text-sm text-gray-200" />
        </div>
        <div class="flex items-center gap-2">
          <label class="text-xs font-medium uppercase tracking-wide text-gray-400">{t('hooks.enabled')}</label>
          <button type="button" onclick={() => (formEnabled = !formEnabled)}
            class={`relative inline-flex h-5 w-9 items-center rounded-full transition ${formEnabled ? 'bg-sky-600' : 'bg-gray-600'}`}>
            <span class={`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${formEnabled ? 'translate-x-4' : 'translate-x-1'}`}></span>
          </button>
        </div>
      </div>
      <div class="flex justify-end gap-2 pt-2">
        <button type="button" onclick={() => { showAddForm = false; resetForm(); }}
          class="rounded-lg border border-gray-600 bg-gray-800 px-3 py-2 text-sm text-gray-200 hover:bg-gray-700">
          {t('hooks.cancel')}
        </button>
        <button type="button" onclick={addHook}
          class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500">
          {t('hooks.save')}
        </button>
      </div>
    </div>
  {/if}

  {#if loading}
    <p class="text-sm text-gray-400">{t('hooks.loading')}</p>
  {:else if errorMessage}
    <p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-300">
      {errorMessage}
    </p>
  {:else if hooks.length === 0}
    <p class="rounded-xl border border-gray-700 bg-gray-800 px-4 py-3 text-sm text-gray-300">
      {t('hooks.noHooks')}
    </p>
  {:else}
    <div class="space-y-3">
      {#each hooks as hook (hook.id)}
        <article class="rounded-xl border border-gray-700 bg-gray-800 p-4">
          {#if editing === hook.id}
            <div class="space-y-3">
              <div class="grid gap-3 sm:grid-cols-2">
                <div>
                  <label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-400">{t('hooks.event')}</label>
                  <select bind:value={formEvent} class="w-full rounded-lg border border-gray-600 bg-gray-900 px-3 py-2 text-sm text-gray-200">
                    {#each HOOK_EVENTS as ev}
                      <option value={ev}>{humanizeEvent(ev)}</option>
                    {/each}
                  </select>
                </div>
                <div>
                  <label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-400">{t('hooks.timeout')}</label>
                  <input type="number" bind:value={formTimeout} min="1000" step="1000"
                    class="w-full rounded-lg border border-gray-600 bg-gray-900 px-3 py-2 text-sm text-gray-200" />
                </div>
                <div class="sm:col-span-2">
                  <label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-400">{t('hooks.command')}</label>
                  <input type="text" bind:value={formCommand}
                    class="w-full rounded-lg border border-gray-600 bg-gray-900 px-3 py-2 text-sm text-gray-200" />
                </div>
                <div class="flex items-center gap-2">
                  <label class="text-xs font-medium uppercase tracking-wide text-gray-400">{t('hooks.enabled')}</label>
                  <button type="button" onclick={() => (formEnabled = !formEnabled)}
                    class={`relative inline-flex h-5 w-9 items-center rounded-full transition ${formEnabled ? 'bg-sky-600' : 'bg-gray-600'}`}>
                    <span class={`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${formEnabled ? 'translate-x-4' : 'translate-x-1'}`}></span>
                  </button>
                </div>
              </div>
              <div class="flex justify-end gap-2">
                <button type="button" onclick={cancelEdit}
                  class="rounded-lg border border-gray-600 bg-gray-800 px-3 py-2 text-sm text-gray-200 hover:bg-gray-700">
                  {t('hooks.cancel')}
                </button>
                <button type="button" onclick={() => saveEdit(hook.id)}
                  class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500">
                  {t('hooks.save')}
                </button>
              </div>
            </div>
          {:else}
            <div class="flex items-start justify-between gap-3">
              <div class="min-w-0 flex-1">
                <div class="flex items-center gap-3">
                  <h3 class="text-lg font-semibold text-gray-100">{humanizeEvent(hook.event)}</h3>
                  <span
                    class={`rounded-full px-2 py-1 text-xs font-medium ${
                      hook.enabled
                        ? 'border border-green-500/50 bg-green-500/20 text-green-300'
                        : 'border border-red-500/50 bg-red-500/20 text-red-300'
                    }`}
                  >
                    {hook.enabled ? t('common.enabled') : t('common.disabled')}
                  </span>
                </div>
                <p class="mt-2 font-mono text-sm text-gray-400">{hook.command}</p>
                <p class="mt-1 text-xs text-gray-500">{t('hooks.timeout')}: {hook.timeout_ms}ms</p>
              </div>
              <div class="flex items-center gap-2">
                <button type="button" onclick={() => toggleHook(hook.id)}
                  class={`relative inline-flex h-5 w-9 items-center rounded-full transition ${hook.enabled ? 'bg-sky-600' : 'bg-gray-600'}`}>
                  <span class={`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${hook.enabled ? 'translate-x-4' : 'translate-x-1'}`}></span>
                </button>
                <button type="button" onclick={() => startEdit(hook)}
                  class="rounded-lg border border-gray-600 bg-gray-800 px-2 py-1 text-xs text-gray-300 hover:bg-gray-700">
                  {t('hooks.edit')}
                </button>
                <button type="button" onclick={() => deleteHook(hook.id)}
                  class="rounded-lg border border-red-500/50 bg-red-500/10 px-2 py-1 text-xs text-red-300 hover:bg-red-500/20">
                  {t('hooks.delete')}
                </button>
              </div>
            </div>
          {/if}
        </article>
      {/each}
    </div>
  {/if}
</section>
