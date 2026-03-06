<script>
  import ConfigValue from './ConfigValue.svelte';
  import { t } from './i18n';

  let { value } = $props();

  const primitiveArray = $derived(
    Array.isArray(value) && value.every((item) => !Array.isArray(item) && !isPlainObject(item))
  );

  const objectEntries = $derived(
    isPlainObject(value) ? Object.entries(value).sort(([a], [b]) => a.localeCompare(b)) : []
  );

  function isPlainObject(target) {
    return target !== null && typeof target === 'object' && !Array.isArray(target);
  }

  function isRedacted(target) {
    if (typeof target === 'string') {
      return target.trim() === '***';
    }

    if (Array.isArray(target)) {
      return target.length > 0 && target.every((item) => isRedacted(item));
    }

    if (isPlainObject(target)) {
      const values = Object.values(target);
      return values.length > 0 && values.every((item) => isRedacted(item));
    }

    return false;
  }

  function humanizeKey(key) {
    if (typeof key !== 'string' || key.length === 0) {
      return t('common.unknown');
    }

    const withSpaces = key
      .replaceAll(/([a-z0-9])([A-Z])/g, '$1 $2')
      .replaceAll('_', ' ')
      .trim();

    const acronymMap = {
      api: 'API',
      url: 'URL',
      id: 'ID',
      ui: 'UI',
      ttl: 'TTL',
      cpu: 'CPU',
      gpu: 'GPU',
      tcp: 'TCP',
      tls: 'TLS',
      http: 'HTTP',
      https: 'HTTPS',
      ws: 'WS'
    };

    return withSpaces
      .split(/\s+/)
      .map((part) => {
        const lower = part.toLowerCase();
        if (acronymMap[lower]) {
          return acronymMap[lower];
        }

        return lower.charAt(0).toUpperCase() + lower.slice(1);
      })
      .join(' ');
  }

  function formatPrimitive(target) {
    if (target === null || target === undefined) {
      return t('common.na');
    }

    if (typeof target === 'boolean') {
      return target ? t('common.yes') : t('common.no');
    }

    if (typeof target === 'string') {
      return target.length > 0 ? target : t('common.empty');
    }

    return String(target);
  }
</script>

{#if isRedacted(value)}
  <span class="inline-flex items-center gap-2 rounded-full border border-amber-500/40 bg-amber-500/10 px-2.5 py-1 text-xs font-medium text-amber-200">
    <span aria-hidden="true">🔒</span>
    <span>•••</span>
  </span>
{:else if typeof value === 'boolean'}
  <span
    class={`inline-flex rounded-full border px-2.5 py-1 text-xs font-medium ${
      value
        ? 'border-green-500/50 bg-green-500/20 text-green-300'
        : 'border-red-500/50 bg-red-500/20 text-red-300'
    }`}
  >
    {value ? t('common.enabled') : t('common.disabled')}
  </span>
{:else if value === null || value === undefined}
  <span class="text-sm text-gray-500">{t('config.field.notSet')}</span>
{:else if Array.isArray(value)}
  {#if value.length === 0}
    <span class="text-sm text-gray-500">{t('common.empty')}</span>
  {:else if primitiveArray}
    <span class="text-sm text-gray-100 break-all">{value.map((item) => formatPrimitive(item)).join(', ')}</span>
  {:else}
    <div class="space-y-2">
      {#each value as item, index}
        <div class="rounded-lg border border-gray-700/80 bg-gray-900/70 p-2">
          <p class="mb-1 text-xs text-gray-400">#{index + 1}</p>
          <ConfigValue value={item} />
        </div>
      {/each}
    </div>
  {/if}
{:else if isPlainObject(value)}
  {#if objectEntries.length === 0}
    <span class="text-sm text-gray-500">{t('config.emptyObject')}</span>
  {:else}
    <div class="space-y-2">
      {#each objectEntries as [key, nestedValue]}
        <div class="rounded-lg border border-gray-700/80 bg-gray-900/70 p-2">
          <p class="text-xs font-medium uppercase tracking-wide text-gray-400">{humanizeKey(key)}</p>
          <div class="mt-1 text-sm text-gray-100">
            <ConfigValue value={nestedValue} />
          </div>
        </div>
      {/each}
    </div>
  {/if}
{:else}
  <span class="text-sm text-gray-100 break-all">{formatPrimitive(value)}</span>
{/if}
