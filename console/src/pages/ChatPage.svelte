<script>
  import { onDestroy, tick } from 'svelte';
  import { ApiError, api } from '../lib/api';
  import { createJobWatch } from '../lib/job-watch';
  import { t } from '../lib/i18n';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import StateBlock from '../lib/ui/StateBlock.svelte';
  import ConfirmDialog from '../lib/ui/ConfirmDialog.svelte';
  import { navigate } from '../lib/router';
  import { PIN_GAP_PX, isTerminalPhase, sameOriginApiPath } from '../lib/transport';
  import { Paperclip } from '@lucide/svelte';
  import DecisionApprovalPanel from '../components/DecisionApprovalPanel.svelte';
  import TurnProgress from '../components/TurnProgress.svelte';

  const MAX_FILES = 10;
  const MESSAGES_PAGE_SIZE = 40;
  const LOAD_MORE_SCROLL_THRESHOLD = 80;
  const IMAGE_VIDEO_MARKER_REGEX =
    /\[(IMAGE|VIDEO):([^\]]+)\]|(data:(?:image|video)\/[a-zA-Z0-9.+-]+;base64,[a-zA-Z0-9+/=]+)/gi;

  let { sessionId = '' } = $props();

  let messages = $state([]);
  let draftMessage = $state('');
  let loading = $state(true);
  let sending = $state(false);
  let loadingMore = $state(false);
  let hasMore = $state(false);
  let loadedCount = $state(0);
  let errorMessage = $state('');
  let scrollContainer = $state(null);
  let fileInput = $state(null);
  let selectedFiles = $state([]);
  let dragActive = $state(false);
  let dragDepth = 0;
  let turn = $state(null);
  let turnUncertain = $state(false);
  let cancelOpen = $state(false);
  let watchHandle = null;

  function pinnedToBottom() {
    if (!scrollContainer) {
      return true;
    }
    const gap = scrollContainer.scrollHeight - scrollContainer.scrollTop - scrollContainer.clientHeight;
    return gap < PIN_GAP_PX;
  }

  async function followIfPinned(wasPinned) {
    await tick();
    if (wasPinned && scrollContainer) {
      scrollContainer.scrollTop = scrollContainer.scrollHeight;
    }
  }

  function applyReply(body) {
    const reply = body && typeof body.reply === 'string' ? body.reply : '';
    if (reply.length === 0) {
      return;
    }
    const wasPinned = pinnedToBottom();
    messages = [...messages, { role: 'assistant', content: reply }];
    followIfPinned(wasPinned);
  }

  function stopWatch() {
    watchHandle?.stop();
    watchHandle = null;
  }

  function startWatch(accepted, originSession) {
    stopWatch();
    const pollPath = sameOriginApiPath(accepted.poll_url) || `/api/jobs/${encodeURIComponent(accepted.job_id)}`;
    const cancelPath = sameOriginApiPath(accepted.cancel_url) || `/api/jobs/${encodeURIComponent(accepted.job_id)}/cancel`;
    turn = {
      sessionId: originSession,
      phase: 'accepted',
      elapsedMs: 0,
      jobId: accepted.job_id,
      workId: typeof accepted.work_id === 'string' ? accepted.work_id : '',
      cancelPath,
      stillRunning: false,
      pollStale: false,
      ack: ''
    };
    watchHandle = createJobWatch({
      poll: () => api.getJobByPath(pollPath),
      onUpdate(update) {
        if (!turn || turn.sessionId !== originSession || sessionId !== originSession) {
          return;
        }
        if (update.error) {
          turn = { ...turn, pollStale: true };
          return;
        }
        const snapshot = update.snapshot || {};
        const phase = typeof snapshot.status === 'string' ? snapshot.status : turn.phase;
        turn = {
          ...turn,
          phase,
          elapsedMs: Number.isFinite(snapshot.elapsed_ms) ? snapshot.elapsed_ms : turn.elapsedMs,
          stillRunning: update.stillRunning === true,
          pollStale: false
        };
        if (phase === 'succeeded') {
          applyReply(snapshot.result);
        } else if (phase === 'failed') {
          errorMessage = typeof snapshot.error === 'string' && snapshot.error.length > 0 ? snapshot.error : t('chat.failed');
        }
      }
    });
  }

  function messageIdentity(message) {
    if (message?.message_id) {
      return `id:${message.message_id}`;
    }

    return `fallback:${message?.timestamp ?? ''}:${message?.role ?? ''}:${message?.content ?? ''}`;
  }

  function mergeMessages(nextMessages) {
    const seen = new Set();
    const merged = [];

    for (const message of nextMessages) {
      const identity = messageIdentity(message);
      if (seen.has(identity)) {
        continue;
      }
      seen.add(identity);
      merged.push(message);
    }

    return merged;
  }

  function goBack() {
    navigate('/sessions');
  }

  function messageClass(role) {
    if (role === 'user') {
      return 'ml-auto max-w-[85%] rounded-2xl rounded-br-md bg-blue-600 px-4 py-2 text-white';
    }

    if (role === 'assistant') {
      return 'mr-auto max-w-[85%] rounded-2xl rounded-bl-md bg-gray-200 px-4 py-2 text-gray-900 dark:bg-gray-700 dark:text-gray-100';
    }

    return 'mx-auto max-w-[90%] rounded-lg bg-gray-100/60 px-3 py-1.5 text-center text-xs text-gray-500 dark:bg-gray-800/60 dark:text-gray-400';
  }

  function isImageFile(file) {
    return (file?.type || '').startsWith('image/');
  }

  function isVideoFile(file) {
    return (file?.type || '').startsWith('video/');
  }

  function formatFileSize(size) {
    if (!Number.isFinite(size) || size <= 0) {
      return `0 ${t('common.fileSizeUnits.b')}`;
    }
    const units = ['b', 'kb', 'mb', 'gb'];
    let value = size;
    let index = 0;
    while (value >= 1024 && index < units.length - 1) {
      value /= 1024;
      index += 1;
    }
    return `${value.toFixed(index === 0 ? 0 : 1)} ${t(`common.fileSizeUnits.${units[index]}`)}`;
  }

  function sanitizeFileType(type) {
    if (typeof type === 'string' && type.trim().length > 0) {
      return type;
    }
    return t('common.fileTypeUnknown');
  }

  function createSelectedFileRecord(file) {
    const isImage = isImageFile(file);
    const isVideo = isVideoFile(file);
    return {
      id: `${file.name}-${file.lastModified}-${Math.random().toString(36).slice(2)}`,
      file,
      name: file.name,
      size: file.size,
      type: sanitizeFileType(file.type),
      isImage,
      isVideo,
      previewUrl: isImage || isVideo ? URL.createObjectURL(file) : ''
    };
  }

  function revokePreview(record) {
    if (record && typeof record.previewUrl === 'string' && record.previewUrl.startsWith('blob:')) {
      URL.revokeObjectURL(record.previewUrl);
    }
  }

  function clearSelectedFiles() {
    for (const record of selectedFiles) {
      revokePreview(record);
    }
    selectedFiles = [];
    if (fileInput) {
      fileInput.value = '';
    }
  }

  function addFiles(fileList) {
    if (!fileList || fileList.length === 0 || sending) {
      return;
    }

    const incoming = Array.from(fileList);
    const next = [];
    const slotsLeft = Math.max(0, MAX_FILES - selectedFiles.length);
    for (const file of incoming.slice(0, slotsLeft)) {
      next.push(createSelectedFileRecord(file));
    }
    selectedFiles = [...selectedFiles, ...next];
  }

  function removeFile(recordId) {
    const target = selectedFiles.find((record) => record.id === recordId);
    if (target) {
      revokePreview(target);
    }
    selectedFiles = selectedFiles.filter((record) => record.id !== recordId);
  }

  function openFilePicker() {
    if (sending) {
      return;
    }
    fileInput?.click();
  }

  function handleFileChange(event) {
    addFiles(event.currentTarget?.files);
    if (fileInput) {
      fileInput.value = '';
    }
  }

  function handleDragEnter(event) {
    event.preventDefault();
    if (sending) {
      return;
    }
    dragDepth += 1;
    dragActive = true;
  }

  function handleDragOver(event) {
    event.preventDefault();
    if (!sending && event.dataTransfer) {
      event.dataTransfer.dropEffect = 'copy';
    }
  }

  function handleDragLeave(event) {
    event.preventDefault();
    dragDepth = Math.max(0, dragDepth - 1);
    if (dragDepth === 0) {
      dragActive = false;
    }
  }

  function handleDrop(event) {
    event.preventDefault();
    dragDepth = 0;
    dragActive = false;
    addFiles(event.dataTransfer?.files);
  }

  function handleScroll() {
    if (!scrollContainer || loading || sending || loadingMore || !hasMore) {
      return;
    }

    if (scrollContainer.scrollTop <= LOAD_MORE_SCROLL_THRESHOLD) {
      loadOlderMessages();
    }
  }

  function resolveMediaSource(value) {
    const source = (value || '').trim();
    if (!source) {
      return '';
    }
    const lower = source.toLowerCase();
    if (
      lower.startsWith('data:image/') ||
      lower.startsWith('data:video/') ||
      lower.startsWith('http://') ||
      lower.startsWith('https://')
    ) {
      return source;
    }
    return api.getSessionMediaUrl(source);
  }

  function inferMediaType(markerType, value) {
    const normalized = (value || '').trim().toLowerCase();
    if (markerType === 'VIDEO' || normalized.startsWith('data:video/')) {
      return 'video';
    }
    if (normalized.startsWith('data:image/')) {
      return 'image';
    }
    const videoExtensions = ['.mp4', '.webm', '.mov', '.m4v', '.ogg'];
    if (videoExtensions.some((extension) => normalized.endsWith(extension))) {
      return 'video';
    }
    return 'image';
  }

  function parseMessageContent(content) {
    if (typeof content !== 'string' || content.length === 0) {
      return [];
    }

    const segments = [];
    IMAGE_VIDEO_MARKER_REGEX.lastIndex = 0;
    let cursor = 0;
    let match;
    while ((match = IMAGE_VIDEO_MARKER_REGEX.exec(content)) !== null) {
      if (match.index > cursor) {
        segments.push({
          id: `text-${cursor}`,
          kind: 'text',
          value: content.slice(cursor, match.index)
        });
      }

      const markerType = (match[1] || '').toUpperCase();
      const markerValue = (match[2] || match[3] || '').trim();
      if (markerValue) {
        const mediaType = inferMediaType(markerType, markerValue);
        segments.push({
          id: `${mediaType}-${match.index}`,
          kind: mediaType,
          value: markerValue
        });
      }
      cursor = IMAGE_VIDEO_MARKER_REGEX.lastIndex;
    }

    if (cursor < content.length) {
      segments.push({
        id: `text-tail-${cursor}`,
        kind: 'text',
        value: content.slice(cursor)
      });
    }

    return segments;
  }

  async function scrollToBottom() {
    await tick();
    if (scrollContainer) {
      scrollContainer.scrollTop = scrollContainer.scrollHeight;
    }
  }

  async function loadMessagesPage(offset, { appendOlder = false } = {}) {
    const response = await api.getSessionMessages(sessionId, {
      limit: MESSAGES_PAGE_SIZE + 1,
      offset
    });
    const rawMessages = Array.isArray(response) ? response : [];
    const nextHasMore = rawMessages.length > MESSAGES_PAGE_SIZE;
    const pageMessages = nextHasMore ? rawMessages.slice(0, MESSAGES_PAGE_SIZE) : rawMessages;

    if (appendOlder && scrollContainer) {
      const previousHeight = scrollContainer.scrollHeight;
      messages = mergeMessages([...pageMessages, ...messages]);
      loadedCount = messages.length;
      hasMore = nextHasMore;
      await tick();
      scrollContainer.scrollTop = scrollContainer.scrollHeight - previousHeight;
      return;
    }

    messages = mergeMessages(pageMessages);
    loadedCount = messages.length;
    hasMore = nextHasMore;
  }

  async function loadInitialMessages() {
    try {
      await loadMessagesPage(0);
      errorMessage = '';
      await scrollToBottom();
    } catch (error) {
      errorMessage = error instanceof Error ? error.message : t('chat.loadFailed');
    } finally {
      loading = false;
    }
  }

  async function loadOlderMessages() {
    if (loadingMore || !hasMore) {
      return;
    }

    loadingMore = true;
    try {
      await loadMessagesPage(loadedCount, { appendOlder: true });
      errorMessage = '';
    } catch (error) {
      errorMessage = error instanceof Error ? error.message : t('chat.loadFailed');
    } finally {
      loadingMore = false;
    }
  }

  async function refreshPreservingPin() {
    const wasPinned = pinnedToBottom();
    await loadMessagesPage(0);
    errorMessage = '';
    await followIfPinned(wasPinned);
  }

  async function refreshMessages() {
    try {
      await loadMessagesPage(0);
      errorMessage = '';
      await scrollToBottom();
    } catch (error) {
      errorMessage = error instanceof Error ? error.message : t('chat.loadFailed');
    }
  }

  function turnActive() {
    return turn && !isTerminalPhase(turn.phase);
  }

  async function confirmCancel() {
    const current = turn;
    cancelOpen = false;
    if (!current?.cancelPath) {
      return;
    }
    try {
      const receipt = await api.postByApiPath(current.cancelPath);
      const targets = Array.isArray(receipt?.targets) ? receipt.targets.join(', ') : '';
      if (turn && turn.jobId === current.jobId) {
        turn = {
          ...turn,
          ack: `${t('chat.cancelAck')} ${receipt?.job_id || current.jobId} ${receipt?.work_id || current.workId} ${targets}`.trim()
        };
      }
    } catch (error) {
      if (error instanceof ApiError) {
        errorMessage = error.message;
      } else if (turn && turn.jobId === current.jobId) {
        turnUncertain = true;
      }
    }
  }

  async function sendMessage() {
    const message = draftMessage.trim();
    const files = selectedFiles.map((record) => record.file);
    if ((message.length === 0 && files.length === 0) || sending || turnActive()) {
      return;
    }

    sending = true;
    errorMessage = '';
    turnUncertain = false;
    const originSession = sessionId;

    try {
      const response = files.length > 0
        ? await api.sendTurnWithMedia(originSession, message, files)
        : await api.sendTurn(originSession, message);
      if (sessionId !== originSession) {
        return;
      }
      const status = response?.status;
      const body = response?.body;
      const accepted = status === 202 && body?.status === 'accepted' && typeof body.job_id === 'string' && body.job_id.length > 0;
      const synchronous = Number.isInteger(status) && status >= 200 && status < 300 && !accepted && body && typeof body.reply === 'string';
      if (!accepted && !synchronous) {
        turnUncertain = true;
        return;
      }
      draftMessage = '';
      clearSelectedFiles();
      if (message.length > 0 && files.length === 0) {
        const wasPinned = pinnedToBottom();
        messages = [...messages, { role: 'user', content: message }];
        await followIfPinned(wasPinned);
      } else if (files.length > 0) {
        const wasPinned = pinnedToBottom();
        await loadMessagesPage(0);
        await followIfPinned(wasPinned);
      }
      if (accepted) {
        startWatch(body, originSession);
        return;
      }
      applyReply(body);
    } catch (error) {
      if (sessionId !== originSession) {
        return;
      }
      if (error instanceof ApiError) {
        errorMessage = error.message;
      } else {
        turnUncertain = true;
      }
    } finally {
      sending = false;
    }
  }

  function handleSubmit(event) {
    event.preventDefault();
    sendMessage();
  }

  $effect(() => {
    let cancelled = false;

    const refresh = async () => {
      if (cancelled) {
        return;
      }
      loading = true;
      await loadInitialMessages();
    };

    refresh();

    return () => {
      cancelled = true;
      stopWatch();
      turn = null;
    };
  });

  onDestroy(() => {
    stopWatch();
    for (const record of selectedFiles) {
      revokePreview(record);
    }
  });
</script>

<section class="flex h-[calc(100vh-10rem)] flex-col gap-4">
  {#key sessionId}
    <DecisionApprovalPanel {sessionId} onsettled={refreshPreservingPin} />
  {/key}
  <div class="flex items-center justify-between">
    <div class="min-w-0">
      <PageHeader title={t('chat.title')} />
      <p class="truncate font-mono text-xs text-gray-500 dark:text-gray-400">{t('chat.session')}: {sessionId}</p>
    </div>
    <button
      type="button"
      class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
      onclick={goBack}
    >
      {t('chat.back')}
    </button>
  </div>

  {#if errorMessage}
    <StateBlock variant="error" message={errorMessage} />
  {/if}
  {#if turnUncertain}
    <StateBlock variant="disconnected-stale" message={t('chat.uncertainSend')} />
  {/if}
  {#if turn}
    <TurnProgress
      phase={turn.phase}
      elapsedMs={turn.elapsedMs}
      jobId={turn.jobId}
      workId={turn.workId}
      stillRunning={turn.stillRunning}
      pollStale={turn.pollStale}
      ack={turn.ack}
      onCancel={() => { cancelOpen = true; }}
      onCheckAgain={() => watchHandle?.checkAgain()}
    />
  {/if}

  <div
    class="flex min-h-0 flex-1 flex-col rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"
    role="region"
    aria-label={t('chat.messagesRegion')}
    ondragenter={handleDragEnter}
    ondragover={handleDragOver}
    ondragleave={handleDragLeave}
    ondrop={handleDrop}
  >
    <div
      class={`ui-chat-scroll flex-1 overflow-y-auto p-4 ${dragActive ? 'bg-blue-500/10 ring-1 ring-inset ring-blue-500/40' : ''}`}
      bind:this={scrollContainer}
      onscroll={handleScroll}
    >
      {#if dragActive}
        <p class="mb-3 rounded-lg border border-blue-500/40 bg-blue-500/15 px-3 py-2 text-sm text-blue-700 dark:text-blue-200">
          {t('chat.dropFiles', { count: selectedFiles.length, max: MAX_FILES })}
        </p>
      {/if}
      {#if loading}
        <StateBlock variant="loading" message={t('chat.loading')} />
      {:else if messages.length === 0}
        <StateBlock variant="empty" message={t('chat.empty')} />
      {:else}
        <div class="space-y-3">
          {#if hasMore}
            <div class="flex justify-center">
              <button
                type="button"
                onclick={loadOlderMessages}
                disabled={loadingMore}
                class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 disabled:cursor-not-allowed disabled:opacity-50 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200 dark:hover:bg-gray-700"
              >
                {loadingMore ? t('chat.loadingMore') : t('chat.loadMore')}
              </button>
            </div>
          {/if}

          {#each messages as message, index (message.message_id ?? `${message.timestamp ?? 'local'}-${index}`)}
            <div class={messageClass(message.role)}>
              {#each parseMessageContent(message.content) as segment (segment.id)}
                {#if segment.kind === 'text'}
                  {#if segment.value.trim().length > 0}
                    <p class="whitespace-pre-wrap break-words text-sm">{segment.value}</p>
                  {/if}
                {:else if segment.kind === 'image'}
                  <img
                    src={resolveMediaSource(segment.value)}
                    alt={t('chat.attachmentAlt')}
                    class="mt-2 max-h-80 max-w-full rounded-lg border border-gray-300/40 object-contain dark:border-gray-600/40"
                    loading="lazy"
                  />
                {:else if segment.kind === 'video'}
                  <!-- svelte-ignore a11y_media_has_caption -->
                  <video
                    src={resolveMediaSource(segment.value)}
                    controls
                    class="mt-2 max-h-80 max-w-full rounded-lg border border-gray-300/40 dark:border-gray-600/40"
                  ></video>
                {/if}
              {/each}
            </div>
          {/each}
        </div>
      {/if}
    </div>

    <form class="border-t border-gray-200 p-3 dark:border-gray-700" onsubmit={handleSubmit}>
      <input
        bind:this={fileInput}
        type="file"
        class="hidden"
        multiple
        onchange={handleFileChange}
        accept="image/*,video/*,.pdf,.doc,.docx,.txt,.md,.csv,.json,.zip,.tar,.gz,.rar,.ppt,.pptx,.xls,.xlsx"
      />

      {#if selectedFiles.length > 0}
        <div class="mb-3 space-y-2 rounded-lg border border-gray-200 bg-gray-50/70 p-2.5 dark:border-gray-700 dark:bg-gray-900/70">
          <p class="text-xs text-gray-600 dark:text-gray-300">{t('chat.attachments', { count: selectedFiles.length, max: MAX_FILES })}</p>
          <div class="max-h-44 space-y-2 overflow-y-auto pr-1">
            {#each selectedFiles as record (record.id)}
              <div class="flex items-center gap-2 rounded-md border border-gray-200 bg-white/90 p-2 dark:border-gray-700 dark:bg-gray-800/90">
                {#if record.isImage}
                  <img
                    src={record.previewUrl}
                    alt={record.name}
                    class="h-12 w-12 rounded border border-gray-300 object-cover dark:border-gray-600"
                  />
                {:else if record.isVideo}
                  <video
                    src={record.previewUrl}
                    class="h-12 w-12 rounded border border-gray-300 object-cover dark:border-gray-600"
                    muted
                  ></video>
                {:else}
                  <div
                    class="flex h-12 w-12 items-center justify-center rounded border border-gray-300 bg-gray-100 text-lg text-gray-700 dark:border-gray-600 dark:bg-gray-700 dark:text-gray-200"
                  >
                    {t('chat.documentFallback')}
                  </div>
                {/if}
                <div class="min-w-0 flex-1">
                  <p class="truncate text-sm text-gray-900 dark:text-gray-100">{record.name}</p>
                  <p class="truncate text-xs text-gray-500 dark:text-gray-400">{record.type} · {formatFileSize(record.size)}</p>
                </div>
                <button
                  type="button"
                  class="rounded px-2 py-1 text-xs text-gray-600 hover:bg-gray-200 hover:text-gray-900 dark:text-gray-300 dark:hover:bg-gray-700 dark:hover:text-white"
                  onclick={() => removeFile(record.id)}
                >
                  {t('chat.removeAttachment')}
                </button>
              </div>
            {/each}
          </div>
        </div>
      {/if}

      <div class="flex items-end gap-2">
        <textarea
          bind:value={draftMessage}
          rows="2"
          placeholder={t('chat.inputPlaceholder')}
          class="min-h-[2.75rem] flex-1 resize-y rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-900 outline-none focus:border-blue-500 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-100"
        ></textarea>
        <button
          type="button"
          title={t('chat.attachFiles')}
          class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-600 hover:border-gray-400 hover:bg-gray-100 disabled:cursor-not-allowed disabled:opacity-50 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:border-gray-500 dark:hover:bg-gray-700"
          onclick={openFilePicker}
          disabled={sending || turnActive() || selectedFiles.length >= MAX_FILES}
        >
          <Paperclip size={16} />
        </button>
        <button
          type="submit"
          class="rounded-lg bg-blue-600 px-4 py-2 text-sm font-medium text-white hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50"
          disabled={sending || turnActive() || (!draftMessage.trim() && selectedFiles.length === 0)}
        >
          {sending ? t('chat.sending') : t('chat.send')}
        </button>
      </div>
    </form>
  </div>
</section>

<ConfirmDialog
  open={cancelOpen}
  title={t('chat.cancelTitle')}
  objectName={turn?.jobId || ''}
  consequence={t('chat.cancelConsequence')}
  onCancel={() => { cancelOpen = false; }}
  onConfirm={confirmCancel}
/>
