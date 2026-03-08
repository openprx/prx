<script>
  import { apiBaseUrl } from '../lib/api';
  import { t } from '../lib/i18n';

  const MAX_LINES = 1000;
  const RECONNECT_BASE_MS = 500;
  const RECONNECT_MAX_MS = 10_000;

  let lines = $state([]);
  let paused = $state(false);
  let connectionStatus = $state('disconnected');
  let logViewport = $state(null);

  let socket = null;
  let reconnectTimer = null;
  let reconnectAttempts = 0;
  let shouldReconnect = true;

  const statusTone = $derived(
    connectionStatus === 'connected'
      ? 'border-green-500/50 bg-green-500/15 text-green-700 dark:text-green-300'
      : connectionStatus === 'reconnecting'
        ? 'border-amber-500/50 bg-amber-500/15 text-amber-700 dark:text-amber-200'
        : 'border-red-500/50 bg-red-500/15 text-red-700 dark:text-red-300'
  );
  const statusLabel = $derived(
    connectionStatus === 'connected'
      ? t('logs.connected')
      : connectionStatus === 'reconnecting'
        ? t('logs.reconnecting')
        : t('logs.disconnected')
  );

  function resolveLogsWsUrl() {
    const baseUrl = apiBaseUrl
      ? new URL(apiBaseUrl, window.location.href)
      : new URL(window.location.href);

    baseUrl.protocol = baseUrl.protocol === 'https:' ? 'wss:' : 'ws:';
    baseUrl.pathname = '/api/logs/stream';
    baseUrl.search = '';
    baseUrl.hash = '';
    return baseUrl.toString();
  }

  function appendLines(rawLine) {
    if (typeof rawLine !== 'string' || rawLine.length === 0) {
      return;
    }

    const incoming = rawLine.split(/\r?\n/).filter((line) => line.length > 0);
    if (incoming.length === 0) {
      return;
    }

    const next = [...lines, ...incoming];
    lines = next.length > MAX_LINES ? next.slice(next.length - MAX_LINES) : next;
  }

  function clearReconnectTimer() {
    if (reconnectTimer !== null) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
  }

  function closeSocket() {
    if (socket) {
      socket.onopen = null;
      socket.onmessage = null;
      socket.onerror = null;
      socket.onclose = null;
      socket.close();
      socket = null;
    }
  }

  function scheduleReconnect() {
    if (!shouldReconnect) {
      connectionStatus = 'disconnected';
      return;
    }

    connectionStatus = 'reconnecting';
    const delay = Math.min(RECONNECT_BASE_MS * 2 ** reconnectAttempts, RECONNECT_MAX_MS);
    reconnectAttempts += 1;

    clearReconnectTimer();
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      connectSocket();
    }, delay);
  }

  function connectSocket() {
    clearReconnectTimer();

    connectionStatus = 'reconnecting';
    closeSocket();

    let nextSocket;
    try {
      nextSocket = new WebSocket(resolveLogsWsUrl());
    } catch {
      scheduleReconnect();
      return;
    }

    socket = nextSocket;

    nextSocket.onopen = () => {
      reconnectAttempts = 0;
      connectionStatus = 'connected';
    };

    nextSocket.onmessage = (event) => {
      if (paused) {
        return;
      }
      appendLines(event.data);
    };

    nextSocket.onerror = () => {
      if (nextSocket.readyState === WebSocket.OPEN || nextSocket.readyState === WebSocket.CONNECTING) {
        nextSocket.close();
      }
    };

    nextSocket.onclose = () => {
      socket = null;
      scheduleReconnect();
    };
  }

  function togglePause() {
    paused = !paused;
  }

  function clearLines() {
    lines = [];
  }

  $effect(() => {
    shouldReconnect = true;
    connectSocket();

    return () => {
      shouldReconnect = false;
      clearReconnectTimer();
      closeSocket();
      connectionStatus = 'disconnected';
    };
  });

  $effect(() => {
    lines.length;
    paused;
    if (paused || !logViewport) {
      return;
    }

    queueMicrotask(() => {
      if (logViewport) {
        logViewport.scrollTop = logViewport.scrollHeight;
      }
    });
  });
</script>

<section class="space-y-4">
  <div class="flex flex-wrap items-center justify-between gap-3">
    <h2 class="text-2xl font-semibold">{t('logs.title')}</h2>
    <div class="flex items-center gap-2">
      <span class={`rounded-full border px-2 py-1 text-xs font-medium uppercase tracking-wide ${statusTone}`}>
        {statusLabel}
      </span>
      <button
        type="button"
        onclick={togglePause}
        class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
      >
        {paused ? t('logs.resume') : t('logs.pause')}
      </button>
      <button
        type="button"
        onclick={clearLines}
        class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
      >
        {t('logs.clear')}
      </button>
    </div>
  </div>

  <div
    bind:this={logViewport}
    class="h-[65vh] overflow-y-auto rounded-xl border border-gray-200 bg-gray-50 p-4 font-mono text-xs leading-5 text-green-800 dark:border-gray-700 dark:bg-gray-950 dark:text-green-300"
  >
    {#if lines.length === 0}
      <p class="text-gray-400 dark:text-gray-500">{t('logs.waiting')}</p>
    {:else}
      <ol class="space-y-1">
        {#each lines as line, index}
          <li class="whitespace-pre-wrap break-words">
            <span class="mr-3 select-none text-gray-400 dark:text-gray-600">{String(index + 1).padStart(4, '0')}</span>
            <span>{line}</span>
          </li>
        {/each}
      </ol>
    {/if}
  </div>
</section>
