<script>
  import ConfigValue from '../lib/ConfigValue.svelte';
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';
  import { highlightJson } from '../lib/jsonHighlight';

  const MODEL_KEYS = new Set([
    'api_key',
    'api_url',
    'default_provider',
    'default_model',
    'default_temperature',
    'reliability',
    'model_routes',
    'embedding_routes',
    'query_classification',
    'agents'
  ]);
  const SECURITY_KEYS = new Set(['security', 'secrets', 'identity_bindings', 'user_policies']);
  const GATEWAY_KEYS = new Set(['gateway']);
  const MEMORY_KEYS = new Set(['memory', 'storage']);
  const CHANNELS_KEYS = new Set(['channels_config']);

  let config = $state(null);
  let status = $state(null);
  let loading = $state(true);
  let errorMessage = $state('');
  let copyMessage = $state('');
  let showRawJson = $state(false);

  const prettyConfig = $derived(config ? JSON.stringify(config, null, 2) : '');
  const highlightedConfig = $derived(highlightJson(prettyConfig));
  const sections = $derived(buildSections(config, status));

  function isPlainObject(target) {
    return target !== null && typeof target === 'object' && !Array.isArray(target);
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

  function channelLabel(name) {
    const key = `channels.names.${name}`;
    const translated = t(key);
    return translated === key ? humanizeKey(name) : translated;
  }

  function labelForTopLevelKey(key) {
    const labelMap = {
      api_key: 'API Key',
      api_url: 'API URL',
      default_provider: 'Default Provider',
      default_model: 'Default Model',
      default_temperature: 'Default Temperature',
      reliability: 'Reliability',
      model_routes: 'Model Routes',
      embedding_routes: 'Embedding Routes',
      query_classification: 'Query Classification',
      agents: 'Delegate Agents',
      gateway: 'Gateway Settings',
      channels_config: 'Channels',
      memory: 'Memory',
      storage: 'Storage',
      security: 'Security',
      secrets: 'Secrets',
      identity_bindings: 'Identity Bindings',
      user_policies: 'User Policies'
    };

    return labelMap[key] ?? humanizeKey(key);
  }

  function isConfiguredChannel(value) {
    if (value === null || value === undefined || value === false) {
      return false;
    }

    if (Array.isArray(value)) {
      return value.length > 0;
    }

    if (isPlainObject(value)) {
      return Object.keys(value).length > 0;
    }

    return true;
  }

  function configuredChannelsFromConfig(channelsConfig) {
    if (!isPlainObject(channelsConfig)) {
      return [];
    }

    return Object.entries(channelsConfig)
      .filter(([, value]) => isConfiguredChannel(value))
      .map(([name]) => channelLabel(name));
  }

  function topLevelEntries(sourceConfig, consumed, keySet) {
    if (!isPlainObject(sourceConfig)) {
      return [];
    }

    const rows = [];
    for (const [key, value] of Object.entries(sourceConfig)) {
      if (!keySet.has(key)) {
        continue;
      }

      consumed.add(key);
      rows.push({ key, label: labelForTopLevelKey(key), value });
    }

    return rows.sort((a, b) => a.label.localeCompare(b.label));
  }

  function buildSections(sourceConfig, sourceStatus) {
    const safeConfig = isPlainObject(sourceConfig) ? sourceConfig : {};
    const consumedKeys = new Set();

    const summaryChannels =
      Array.isArray(sourceStatus?.channels) && sourceStatus.channels.length > 0
        ? sourceStatus.channels.map((name) => channelLabel(name))
        : configuredChannelsFromConfig(safeConfig.channels_config);

    const generalFields = [
      { key: 'version', label: t('config.field.version'), value: sourceStatus?.version ?? null },
      {
        key: 'runtime_model',
        label: t('config.field.runtimeModel'),
        value: sourceStatus?.model ?? safeConfig.default_model ?? null
      },
      {
        key: 'memory_backend',
        label: t('config.field.memoryBackend'),
        value: sourceStatus?.memory_backend ?? safeConfig.memory?.backend ?? null
      },
      {
        key: 'configured_channels',
        label: t('config.field.configuredChannels'),
        value: summaryChannels.length > 0 ? summaryChannels : t('config.field.notConfigured')
      }
    ];

    const gatewayFields = topLevelEntries(safeConfig, consumedKeys, GATEWAY_KEYS);
    const memoryFields = topLevelEntries(safeConfig, consumedKeys, MEMORY_KEYS);
    const securityFields = topLevelEntries(safeConfig, consumedKeys, SECURITY_KEYS);
    const modelFields = topLevelEntries(safeConfig, consumedKeys, MODEL_KEYS);

    const channelsObject = safeConfig.channels_config;
    let channels = [];
    if (isPlainObject(channelsObject)) {
      consumedKeys.add('channels_config');
      channels = Object.entries(channelsObject)
        .sort(([a], [b]) => a.localeCompare(b))
        .map(([name, value]) => ({
          name,
          label: channelLabel(name),
          configured: isConfiguredChannel(value),
          value
        }));
    }

    const otherFields = Object.entries(safeConfig)
      .filter(([key]) => {
        if (CHANNELS_KEYS.has(key)) {
          return false;
        }

        return !consumedKeys.has(key);
      })
      .map(([key, value]) => ({
        key,
        label: labelForTopLevelKey(key),
        value
      }))
      .sort((a, b) => a.label.localeCompare(b.label));

    return [
      {
        id: 'general',
        title: t('config.section.general'),
        fields: generalFields,
        defaultOpen: true
      },
      {
        id: 'gateway',
        title: t('config.section.gateway'),
        fields: gatewayFields,
        defaultOpen: true
      },
      {
        id: 'channels',
        title: t('config.section.channels'),
        channels,
        defaultOpen: true
      },
      {
        id: 'memory',
        title: t('config.section.memory'),
        fields: memoryFields,
        defaultOpen: false
      },
      {
        id: 'security',
        title: t('config.section.security'),
        fields: securityFields,
        defaultOpen: false
      },
      {
        id: 'model',
        title: t('config.section.model'),
        fields: modelFields,
        defaultOpen: false
      },
      {
        id: 'other',
        title: t('config.section.other'),
        fields: otherFields,
        defaultOpen: false
      }
    ];
  }

  async function loadConfig() {
    try {
      const [configResponse, statusResponse] = await Promise.all([
        api.getConfig(),
        api.getStatus().catch(() => null)
      ]);

      config = isPlainObject(configResponse) ? configResponse : {};
      status = statusResponse;
      errorMessage = '';
    } catch (error) {
      errorMessage = error instanceof Error ? error.message : t('config.loadFailed');
    } finally {
      loading = false;
    }
  }

  async function copyToClipboard() {
    if (!prettyConfig || typeof navigator === 'undefined' || !navigator.clipboard) {
      copyMessage = t('common.clipboardUnavailable');
      return;
    }

    try {
      await navigator.clipboard.writeText(prettyConfig);
      copyMessage = t('common.copied');
      setTimeout(() => {
        copyMessage = '';
      }, 1500);
    } catch {
      copyMessage = t('common.copyFailed');
    }
  }

  $effect(() => {
    loadConfig();
  });
</script>

<section class="space-y-6">
  <div class="flex items-center justify-between gap-4">
    <h2 class="text-2xl font-semibold">{t('config.title')}</h2>
    <div class="flex items-center gap-3">
      {#if copyMessage}
        <span class="text-xs text-gray-400">{copyMessage}</span>
      {/if}
      <button
        type="button"
        onclick={() => (showRawJson = !showRawJson)}
        class="rounded-lg border border-gray-600 bg-gray-800 px-3 py-2 text-sm text-gray-200 transition hover:bg-gray-700"
      >
        {showRawJson ? t('config.structured') : t('config.rawJson')}
      </button>
      <button
        type="button"
        onclick={copyToClipboard}
        class="rounded-lg border border-gray-600 bg-gray-800 px-3 py-2 text-sm text-gray-200 transition hover:bg-gray-700"
      >
        {t('config.copyJson')}
      </button>
    </div>
  </div>

  {#if loading}
    <p class="text-sm text-gray-400">{t('config.loading')}</p>
  {:else if errorMessage}
    <p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-300">
      {errorMessage}
    </p>
  {:else if showRawJson}
    <div class="overflow-x-auto rounded-xl border border-gray-700 bg-gray-950 p-4">
      <pre class="text-sm leading-6 text-gray-200"><code>{@html highlightedConfig}</code></pre>
    </div>
  {:else}
    <div class="space-y-4">
      {#each sections as section}
        <details class="rounded-xl border border-gray-700 bg-gray-800 p-4" open={section.defaultOpen}>
          <summary class="cursor-pointer select-none text-base font-semibold text-gray-100">
            {section.title}
          </summary>

          <div class="mt-3 space-y-3">
            {#if section.id === 'channels'}
              {#if !section.channels || section.channels.length === 0}
                <p class="text-sm text-gray-400">{t('config.channel.notConfigured')}</p>
              {:else}
                {#each section.channels as channel}
                  <details
                    class="rounded-lg border border-gray-700 bg-gray-900/60 p-3"
                    open={channel.configured}
                  >
                    <summary class="flex cursor-pointer list-none items-center justify-between gap-3 text-sm font-medium text-gray-200">
                      <span>{channel.label}</span>
                      <span
                        class={`inline-flex rounded-full border px-2 py-1 text-xs ${
                          channel.configured
                            ? 'border-green-500/50 bg-green-500/20 text-green-300'
                            : 'border-gray-600 bg-gray-800 text-gray-300'
                        }`}
                      >
                        {channel.configured ? t('common.enabled') : t('config.channel.notConfigured')}
                      </span>
                    </summary>

                    <div class="mt-2 text-sm text-gray-100">
                      {#if channel.configured}
                        <ConfigValue value={channel.value} />
                      {:else}
                        <p class="text-gray-400">{t('config.channel.notConfigured')}</p>
                      {/if}
                    </div>
                  </details>
                {/each}
              {/if}
            {:else}
              {#if !section.fields || section.fields.length === 0}
                <p class="text-sm text-gray-400">{t('config.emptyObject')}</p>
              {:else}
                <div class="grid gap-3">
                  {#each section.fields as field}
                    <article class="rounded-lg border border-gray-700 bg-gray-900/60 p-3">
                      <p class="text-xs font-medium uppercase tracking-wide text-gray-400">{field.label}</p>
                      <div class="mt-1 text-sm text-gray-100">
                        <ConfigValue value={field.value} />
                      </div>
                    </article>
                  {/each}
                </div>
              {/if}
            {/if}
          </div>
        </details>
      {/each}
    </div>
  {/if}
</section>
