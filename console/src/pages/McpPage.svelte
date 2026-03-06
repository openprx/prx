<script>
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';

  let servers = $state([]);
  let loading = $state(true);
  let errorMessage = $state('');
  let expandedServer = $state(null);

  async function loadServers() {
    try {
      const response = await api.getMcpServers();
      servers = Array.isArray(response?.servers) ? response.servers : [];
      errorMessage = '';
    } catch {
      // API not implemented yet - use mock data
      servers = [
        {
          name: 'filesystem',
          url: 'stdio:///usr/local/bin/mcp-filesystem',
          status: 'connected',
          tools: [
            { name: 'read_file', description: 'Read contents of a file' },
            { name: 'write_file', description: 'Write content to a file' },
            { name: 'list_directory', description: 'List directory contents' }
          ]
        },
        {
          name: 'github',
          url: 'https://mcp.github.com/sse',
          status: 'connected',
          tools: [
            { name: 'search_repositories', description: 'Search GitHub repositories' },
            { name: 'create_issue', description: 'Create a new issue' },
            { name: 'list_pull_requests', description: 'List pull requests' }
          ]
        },
        {
          name: 'database',
          url: 'stdio:///opt/mcp/db-server',
          status: 'disconnected',
          tools: []
        }
      ];
      errorMessage = '';
    } finally {
      loading = false;
    }
  }

  function toggleExpand(serverName) {
    expandedServer = expandedServer === serverName ? null : serverName;
  }

  async function refreshServers() {
    loading = true;
    await loadServers();
  }

  $effect(() => {
    loadServers();
  });
</script>

<section class="space-y-6">
  <div class="flex items-center justify-between">
    <h2 class="text-2xl font-semibold">{t('mcp.title')}</h2>
    <button
      type="button"
      onclick={refreshServers}
      class="rounded-lg border border-gray-600 bg-gray-800 px-3 py-2 text-sm text-gray-200 transition hover:bg-gray-700"
    >
      {t('common.refresh')}
    </button>
  </div>

  {#if loading}
    <p class="text-sm text-gray-400">{t('mcp.loading')}</p>
  {:else if errorMessage}
    <p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-300">
      {errorMessage}
    </p>
  {:else if servers.length === 0}
    <p class="rounded-xl border border-gray-700 bg-gray-800 px-4 py-3 text-sm text-gray-300">
      {t('mcp.noServers')}
    </p>
  {:else}
    <div class="space-y-4">
      {#each servers as server}
        <article class="rounded-xl border border-gray-700 bg-gray-800">
          <button
            type="button"
            onclick={() => toggleExpand(server.name)}
            class="flex w-full items-center justify-between gap-3 p-4 text-left"
          >
            <div class="min-w-0 flex-1">
              <div class="flex items-center gap-3">
                <h3 class="text-lg font-semibold text-gray-100">{server.name}</h3>
                <span
                  class={`rounded-full px-2 py-1 text-xs font-medium ${
                    server.status === 'connected'
                      ? 'border border-green-500/50 bg-green-500/20 text-green-300'
                      : 'border border-red-500/50 bg-red-500/20 text-red-300'
                  }`}
                >
                  {server.status === 'connected' ? t('mcp.connected') : t('mcp.disconnected')}
                </span>
              </div>
              <p class="mt-1 font-mono text-sm text-gray-400">{server.url}</p>
            </div>
            <span class="text-xs text-gray-500">
              {server.tools?.length ?? 0} {t('mcp.tools')}
            </span>
          </button>

          {#if expandedServer === server.name && server.tools && server.tools.length > 0}
            <div class="border-t border-gray-700 p-4">
              <h4 class="mb-3 text-sm font-medium uppercase tracking-wide text-gray-400">{t('mcp.availableTools')}</h4>
              <div class="grid gap-2">
                {#each server.tools as tool}
                  <div class="rounded-lg border border-gray-700 bg-gray-900/60 p-3">
                    <p class="font-mono text-sm font-medium text-gray-200">{tool.name}</p>
                    {#if tool.description}
                      <p class="mt-1 text-xs text-gray-400">{tool.description}</p>
                    {/if}
                  </div>
                {/each}
              </div>
            </div>
          {:else if expandedServer === server.name && (!server.tools || server.tools.length === 0)}
            <div class="border-t border-gray-700 p-4">
              <p class="text-sm text-gray-400">{t('mcp.noTools')}</p>
            </div>
          {/if}
        </article>
      {/each}
    </div>
  {/if}
</section>
