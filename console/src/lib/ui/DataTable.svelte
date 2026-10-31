<script>
  import { t } from '../i18n';
  import StateBlock from './StateBlock.svelte';
  import TruncatedValue from './TruncatedValue.svelte';

  let { columns = [], rows = [], caption = '' } = $props();

  const safeColumns = $derived(Array.isArray(columns) ? columns : []);
  const safeRows = $derived(Array.isArray(rows) ? rows : []);

  function cellText(row, key) {
    const value = row?.[key];
    if (typeof value === 'string' && value.trim().length > 0) {
      return value;
    }
    if (typeof value === 'number' && Number.isFinite(value)) {
      return String(value);
    }
    if (typeof value === 'boolean') {
      return value ? t('common.yes') : t('common.no');
    }
    return t('common.unknown');
  }
</script>

{#if safeRows.length === 0}
  <StateBlock variant="empty" message={t('table.empty')} />
{:else}
  <div class="ui-data-table-scroll">
    <table class="ui-data-table">
      {#if caption}
        <caption>{caption}</caption>
      {/if}
      <thead>
        <tr>
          {#each safeColumns as column (column.key)}
            <th scope="col" class="ui-data-table-head">{column.label}</th>
          {/each}
        </tr>
      </thead>
      <tbody>
        {#each safeRows as row, index (`${row?.id ?? index}`)}
          <tr>
            {#each safeColumns as column (column.key)}
              <td>
                {#if column.truncate}
                  <TruncatedValue value={cellText(row, column.key)} />
                {:else}
                  {cellText(row, column.key)}
                {/if}
              </td>
            {/each}
          </tr>
        {/each}
      </tbody>
    </table>
  </div>
  <div class="ui-data-cards">
    {#each safeRows as row, index (`${row?.id ?? index}`)}
      <article class="ui-data-card">
        {#each safeColumns as column (column.key)}
          <div>
            <p class="ui-data-card-label">{column.label}</p>
            {#if column.truncate}
              <TruncatedValue value={cellText(row, column.key)} />
            {:else}
              <p>{cellText(row, column.key)}</p>
            {/if}
          </div>
        {/each}
      </article>
    {/each}
  </div>
{/if}
