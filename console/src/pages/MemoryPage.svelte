<script>
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';
  import { operationFailure } from '../lib/operation-state';
  import DataTable from '../lib/ui/DataTable.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import StateBlock from '../lib/ui/StateBlock.svelte';

  let phase = $state('loading');
  let message = $state('');
  let hits = $state([]);

  function hitRows(list) {
    return list.map((hit) => ({
      scope: typeof hit?.scope === 'string' ? hit.scope : '',
      source: typeof hit?.source === 'string' ? hit.source : ''
    }));
  }

  $effect(() => {
    let cancelled = false;
    phase = 'loading';
    api
      .searchMemory({ query: 'notes' })
      .then((body) => {
        if (cancelled) {
          return;
        }
        const list = Array.isArray(body?.hits) ? body.hits : Array.isArray(body?.results) ? body.results : [];
        hits = list;
        phase = list.length > 0 ? 'ready' : 'unavailable';
        message = t('operations.memory.unavailable');
      })
      .catch((error) => {
        if (cancelled) {
          return;
        }
        const failure = operationFailure(error);
        phase = failure.variant;
        message = failure.message || t('operations.memory.unavailable');
        hits = [];
      });
    return () => {
      cancelled = true;
    };
  });
</script>

<section class="space-y-4">
  <PageHeader title={t('operations.memory.title')} description={t('operations.memory.unavailable')} />
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('operations.memory.cli')}</p>
  {#if phase === 'loading'}
    <StateBlock variant="loading" />
  {:else if phase === 'ready'}
    <DataTable
      caption={t('operations.memory.title')}
      columns={[
        { key: 'scope', label: 'scope' },
        { key: 'source', label: 'source' }
      ]}
      rows={hitRows(hits)}
    />
  {:else}
    <StateBlock variant={phase} {message} />
  {/if}
</section>
