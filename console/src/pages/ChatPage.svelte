<script>
  import { onDestroy, tick } from 'svelte';
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';
  import { navigate } from '../lib/router';
  import { Paperclip } from '@lucide/svelte';

  const MAX_FILES = 10;
  const IMAGE_VIDEO_MARKER_REGEX =
    /\[(IMAGE|VIDEO):([^\]]+)\]|(data:(?:image|video)\/[a-zA-Z0-9.+-]+;base64,[a-zA-Z0-9+/=]+)/gi;

  let { sessionId = '' } = $props();

  let messages = $state([]);
  let draftMessage = $state('');
  let loading = $state(true);
  let sending = $state(false);
  let errorMessage = $state('');
  let scrollContainer = $state(null);
  let fileInput = $state(null);
  let selectedFiles = $state([]);
  let dragActive = $state(false);
  let dragDepth = 0;

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
      return '0 B';
    }
    const units = ['B', 'KB', 'MB', 'GB'];
    let value = size;
    let index = 0;
    while (value >= 1024 && index < units.length - 1) {
      value /= 1024;
      index += 1;
    }
    return `${value.toFixed(index === 0 ? 0 : 1)} ${units[index]}`;
  }

  function sanitizeFileType(type) {
    if (typeof type === 'string' && type.trim().length > 0) {
      return type;
    }
    return 'unknown';
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

  async function loadMessages() {
    try {
      const response = await api.getSessionMessages(sessionId);
      messages = Array.isArray(response) ? response : [];
      errorMessage = '';
      await scrollToBottom();
    } catch (error) {
      errorMessage = error instanceof Error ? error.message : t('chat.loadFailed');
    } finally {
      loading = false;
    }
  }

  async function sendMessage() {
    const message = draftMessage.trim();
    const files = selectedFiles.map((record) => record.file);
    if ((message.length === 0 && files.length === 0) || sending) {
      return;
    }

    sending = true;
    draftMessage = '';
    errorMessage = '';

    const hasMedia = files.length > 0;
    if (!hasMedia) {
      messages = [...messages, { role: 'user', content: message }];
      await scrollToBottom();
    }

    try {
      const response = hasMedia
        ? await api.sendMessageWithMedia(sessionId, message, files)
        : await api.sendMessage(sessionId, message);
      if (hasMedia) {
        await loadMessages();
      } else if (response && typeof response.reply === 'string' && response.reply.length > 0) {
        messages = [...messages, { role: 'assistant', content: response.reply }];
      }
      clearSelectedFiles();
    } catch (error) {
      errorMessage = error instanceof Error ? error.message : t('chat.sendFailed');
      await loadMessages();
    } finally {
      sending = false;
      await scrollToBottom();
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
      await loadMessages();
    };

    refresh();

    return () => {
      cancelled = true;
    };
  });

  onDestroy(() => {
    for (const record of selectedFiles) {
      revokePreview(record);
    }
  });
</script>

<section class="flex h-[calc(100vh-10rem)] flex-col gap-4">
  <div class="flex items-center justify-between">
    <div class="min-w-0">
      <h2 class="text-2xl font-semibold">{t('chat.title')}</h2>
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
    <p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300">
      {errorMessage}
    </p>
  {/if}

  <div
    class="flex min-h-0 flex-1 flex-col rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"
    role="region"
    aria-label="Chat messages"
    ondragenter={handleDragEnter}
    ondragover={handleDragOver}
    ondragleave={handleDragLeave}
    ondrop={handleDrop}
  >
    <div
      class={`flex-1 overflow-y-auto p-4 ${dragActive ? 'bg-blue-500/10 ring-1 ring-inset ring-blue-500/40' : ''}`}
      bind:this={scrollContainer}
    >
      {#if dragActive}
        <p class="mb-3 rounded-lg border border-blue-500/40 bg-blue-500/15 px-3 py-2 text-sm text-blue-700 dark:text-blue-200">
          Drop files to attach ({selectedFiles.length}/{MAX_FILES} selected)
        </p>
      {/if}
      {#if loading}
        <p class="text-sm text-gray-500 dark:text-gray-400">{t('chat.loading')}</p>
      {:else if messages.length === 0}
        <p class="text-sm text-gray-500 dark:text-gray-400">{t('chat.empty')}</p>
      {:else}
        <div class="space-y-3">
          {#each messages as message}
            <div class={messageClass(message.role)}>
              {#each parseMessageContent(message.content) as segment (segment.id)}
                {#if segment.kind === 'text'}
                  {#if segment.value.trim().length > 0}
                    <p class="whitespace-pre-wrap break-words text-sm">{segment.value}</p>
                  {/if}
                {:else if segment.kind === 'image'}
                  <img
                    src={resolveMediaSource(segment.value)}
                    alt="Attachment"
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
          <p class="text-xs text-gray-600 dark:text-gray-300">Attachments ({selectedFiles.length}/{MAX_FILES})</p>
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
                    DOC
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
                  Remove
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
          title="Attach files"
          class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-600 hover:border-gray-400 hover:bg-gray-100 disabled:cursor-not-allowed disabled:opacity-50 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:border-gray-500 dark:hover:bg-gray-700"
          onclick={openFilePicker}
          disabled={sending || selectedFiles.length >= MAX_FILES}
        >
          <Paperclip size={16} />
        </button>
        <button
          type="submit"
          class="rounded-lg bg-blue-600 px-4 py-2 text-sm font-medium text-white hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50"
          disabled={sending || (!draftMessage.trim() && selectedFiles.length === 0)}
        >
          {sending ? t('chat.sending') : t('chat.send')}
        </button>
      </div>
    </form>
  </div>
</section>
