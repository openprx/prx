<script>
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';
  import { operationFailure } from '../lib/operation-state';
  import DataTable from '../lib/ui/DataTable.svelte';
  import Field from '../lib/ui/Field.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import StateBlock from '../lib/ui/StateBlock.svelte';

  let query = $state('');
  let recallQuery = $state('');
  let key = $state('');
  let content = $state('');
  let forgetKey = $state('');
  let phase = $state('idle');
  let message = $state('');
  let scope = $state('');
  let source = $state('');
  let hits = $state([]);
  let notice = $state('');

  function applyRead(body) {
    const list = Array.isArray(body?.hits) ? body.hits : [];
    hits = list;
    scope = typeof body?.scope === 'string' ? body.scope : '';
    source = typeof body?.source === 'string' ? body.source : '';
    phase = body?.empty === true || list.length === 0 ? 'empty' : 'ready';
    message = phase === 'empty' ? t('operations.memory.empty') : '';
  }

  function applyFailure(error) {
    const failure = operationFailure(error);
    phase = failure.variant;
    message = failure.message || t('operations.memory.unavailable');
    hits = [];
    scope = '';
    source = '';
  }

  async function search(event) {
    event.preventDefault();
    phase = 'loading';
    notice = '';
    try {
      applyRead(await api.searchMemory({ query }));
    } catch (error) {
      applyFailure(error);
    }
  }

  async function recall(event) {
    event.preventDefault();
    phase = 'loading';
    notice = '';
    try {
      applyRead(await api.recallMemory({ query: recallQuery }));
    } catch (error) {
      applyFailure(error);
    }
  }

  async function store(event) {
    event.preventDefault();
    notice = '';
    try {
      const body = await api.storeMemory({ key, content });
      scope = typeof body?.scope === 'string' ? body.scope : scope;
      source = typeof body?.source === 'string' ? body.source : source;
      notice = t('operations.memory.stored');
      phase = phase === 'idle' ? 'idle' : phase;
    } catch (error) {
      applyFailure(error);
    }
  }

  async function forget(event) {
    event.preventDefault();
    notice = '';
    try {
      await api.forgetMemory({ key: forgetKey });
      notice = t('operations.memory.forgotten');
    } catch (error) {
      applyFailure(error);
    }
  }
</script>

<section class="space-y-4">
  <PageHeader title={t('operations.memory.title')} description={t('operations.memory.description')} />
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('operations.memory.cli')}</p>
  <form class="space-y-3" onsubmit={search}>
    <Field name="memory-query" label={t('operations.memory.query')} value={query} onValue={(value) => (query = value)} />
    <button type="submit" class="rounded-lg bg-sky-600 px-3 py-2 text-sm text-white">{t('operations.memory.search')}</button>
  </form>
  <form class="space-y-3" onsubmit={recall}>
    <Field name="memory-recall" label={t('operations.memory.recallQuery')} value={recallQuery} onValue={(value) => (recallQuery = value)} />
    <button type="submit" class="rounded-lg border border-gray-300 px-3 py-2 text-sm">{t('operations.memory.recall')}</button>
  </form>
  <form class="space-y-3" onsubmit={store}>
    <Field name="memory-key" label={t('operations.memory.key')} value={key} onValue={(value) => (key = value)} />
    <Field name="memory-content" label={t('operations.memory.content')} value={content} onValue={(value) => (content = value)} />
    <button type="submit" class="rounded-lg border border-gray-300 px-3 py-2 text-sm">{t('operations.memory.store')}</button>
  </form>
  <form class="space-y-3" onsubmit={forget}>
    <Field name="memory-forget" label={t('operations.memory.forgetKey')} value={forgetKey} onValue={(value) => (forgetKey = value)} />
    <button type="submit" class="rounded-lg border border-gray-300 px-3 py-2 text-sm">{t('operations.memory.forget')}</button>
  </form>
  {#if scope || source}
    <p class="text-sm text-gray-600 dark:text-gray-300">{t('operations.memory.scope')}: {scope || t('common.unknown')} · {t('operations.memory.source')}: {source || t('common.unknown')}</p>
  {/if}
  {#if notice}
    <p class="text-sm">{notice}</p>
  {/if}
  {#if phase === 'loading'}
    <StateBlock variant="loading" />
  {:else if phase === 'ready'}
    <DataTable
      caption={t('operations.memory.title')}
      columns={[
        { key: 'key', label: t('operations.memory.key') },
        { key: 'content', label: t('operations.memory.content') },
        { key: 'scope', label: t('operations.memory.scope') },
        { key: 'source', label: t('operations.memory.source') }
      ]}
      rows={hits}
    />
  {:else if phase !== 'idle'}
    <StateBlock variant={phase === 'empty' ? 'empty' : phase} {message} />
  {/if}
</section>
