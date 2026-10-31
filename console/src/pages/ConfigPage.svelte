<script>
  import { t } from '../lib/i18n/index.svelte.js';
  import { api } from '../lib/api.js';
  import { confirmationText } from '../lib/change-flow.svelte.js';
  import { loadFailureVariant } from '../lib/load-state';
  import ErrorNotice from '../lib/ErrorNotice.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import StateBlock from '../lib/ui/StateBlock.svelte';

  let files = $state([]);
  let activeFile = $state('');
  let editContent = $state('');
  let originalContent = $state('');
  let loading = $state(true);
  let saving = $state(false);
  let error = $state(null);
  let saveError = $state(null);
  let saveMsg = $state('');
  let mergedJson = $state('');
  let mergedError = $state(null);
  let viewMode = $state('files'); // 'files' | 'merged'
  // The revision the files were loaded at. Saves send it as If-Match, so a
  // change made after loading answers 409 instead of being overwritten by
  // the stale editor buffer.
  let loadedRevision = $state(null);

  const hasChanges = $derived(editContent !== originalContent);
  // A file that holds a secret is served redacted and read-only
  // (`editable: false`): its credentials are set on the Providers page or
  // through the write-only credential endpoints, never by saving the
  // masked text back.
  const activeEditable = $derived(files.find((f) => f.filename === activeFile)?.editable === true);
  const activeRedacted = $derived(files.find((f) => f.filename === activeFile)?.redacted === true);

  async function loadFiles() {
    loading = true;
    error = null;
    try {
      const { files: result, revision } = await api.getConfigFilesWithRevision();
      files = Array.isArray(result) ? result : (result?.files ?? []);
      loadedRevision = revision;
      if (files.length > 0 && !activeFile) {
        activeFile = files[0].filename;
        editContent = files[0].content;
        originalContent = files[0].content;
      }
    } catch (e) {
      error = e;
    }
    loading = false;
  }

  async function loadMerged() {
    try {
      const config = await api.getConfig();
      mergedJson = JSON.stringify(config, null, 2);
      mergedError = null;
    } catch (e) {
      mergedJson = '';
      mergedError = e;
    }
  }

  function selectFile(filename) {
    const file = files.find(f => f.filename === filename);
    if (!file) return;
    activeFile = filename;
    editContent = file.content;
    originalContent = file.content;
    saveMsg = '';
    saveError = null;
  }

  async function saveFile() {
    if (!activeFile || !hasChanges || !activeEditable) return;
    saving = true;
    saveMsg = '';
    saveError = null;
    try {
      const saved = await saveConfirmingHighRisk(activeFile, editContent);
      originalContent = editContent;
      const idx = files.findIndex(f => f.filename === activeFile);
      if (idx >= 0) files[idx] = { ...files[idx], content: editContent };
      saveMsg = saved?.restart_required?.length > 0 ? t('config.saveRestartRequired') : t('config.saveSuccess');
      setTimeout(() => saveMsg = '', 2000);
    } catch (e) {
      saveError = e;
    }
    saving = false;
  }

  // A high-risk change (credential, encryption off, provider endpoint) is
  // refused with 428 until confirmed: show the server's reasons (paths only)
  // and retry once with the confirmation if the operator agrees. The retry
  // is bound to the revision the refused plan was computed against
  // (`If-Match`): if the files change while the dialog is open, it answers
  // 409 instead of applying risks the operator did not see.
  async function saveConfirmingHighRisk(filename, content) {
    let saved;
    try {
      saved = await api.saveConfigFile(filename, content, { revision: loadedRevision });
    } catch (e) {
      const revision = e.body?.revision;
      if (e?.status !== 428 || !revision || !window.confirm(confirmationText(e))) {
        throw e;
      }
      saved = await api.saveConfigFile(filename, content, { confirmHighRisk: true, revision });
    }
    // The files now hold this save: the next save is based on it.
    loadedRevision = saved?.desired_source_revision ?? null;
    return saved;
  }

  function revert() {
    editContent = originalContent;
  }

  $effect(() => {
    loadFiles();
  });
</script>

<div class="config-page">
  <div class="config-header">
    <PageHeader title={t('config.advancedTitle')} />
    <div class="view-tabs">
      <button
        class="tab-btn" class:active={viewMode === 'files'}
        onclick={() => { viewMode = 'files'; }}
      >{t('config.files')}</button>
      <button
        class="tab-btn" class:active={viewMode === 'merged'}
        onclick={() => { viewMode = 'merged'; loadMerged(); }}
      >{t('config.merged')}</button>
    </div>
  </div>

  <p class="advanced-hint">{t('config.advancedHint')}</p>

  {#if loading}
    <StateBlock variant="loading" message={t('common.loading')} />
  {:else if error}
    <StateBlock variant={loadFailureVariant(error)} message={error?.message || t('config.loadFailed')} />
  {:else if files.length === 0}
    <StateBlock variant="empty" message={t('config.noFiles')} />
  {:else if viewMode === 'files'}
    <div class="editor-layout">
      <div class="file-list">
        {#each files as file}
          <button
            class="file-item" class:active={activeFile === file.filename}
            onclick={() => selectFile(file.filename)}
          >
            <span class="file-name">{file.filename}</span>
            <span class="file-source">{file.source}{file.editable === false ? ` · ${t('config.readOnlyBadge')}` : ''}</span>
          </button>
        {/each}
      </div>
      <div class="editor-area">
        <div class="editor-toolbar">
          <span class="active-filename">{activeFile}</span>
          <div class="toolbar-actions">
            {#if saveMsg}
              <span class="save-msg">{saveMsg}</span>
            {/if}
            <button class="btn secondary" onclick={revert} disabled={!hasChanges || !activeEditable}>
              {t('config.revert')}
            </button>
            <button class="btn primary" onclick={saveFile} disabled={!hasChanges || saving || !activeEditable}>
              {saving ? t('common.saving') : t('config.save')}
            </button>
          </div>
        </div>
        {#if !activeEditable}
          <p class="read-only-note">{activeRedacted ? t('config.readOnlyRedacted') : t('config.readOnly')}</p>
        {/if}
        <ErrorNotice error={saveError} />
        <textarea
          class="toml-editor"
          class:readonly={!activeEditable}
          bind:value={editContent}
          readonly={!activeEditable}
          spellcheck="false"
        ></textarea>
      </div>
    </div>
  {:else}
    <ErrorNotice error={mergedError} />
    <div class="merged-view">
      <textarea class="toml-editor readonly" value={mergedJson} readonly></textarea>
    </div>
  {/if}
</div>

<style>
  .config-page {
    display: flex;
    flex-direction: column;
    height: 100%;
    gap: 0;
  }
  .config-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 16px 24px;
    border-bottom: 1px solid var(--border, #27272a);
  }
  .view-tabs {
    display: flex;
    gap: 4px;
    background: var(--bg-base, #0a0a0b);
    border-radius: 6px;
    padding: 2px;
  }
  .tab-btn {
    padding: 6px 14px;
    border: none;
    background: transparent;
    color: var(--text-secondary, #a1a1aa);
    border-radius: 4px;
    cursor: pointer;
    font-size: 13px;
  }
  .tab-btn.active {
    background: var(--bg-elevated, #18181b);
    color: var(--text-primary, #fafafa);
  }
  .editor-layout {
    display: flex;
    flex: 1;
    min-height: 0;
  }
  .file-list {
    width: 200px;
    border-right: 1px solid var(--border, #27272a);
    overflow-y: auto;
    padding: 8px 0;
    flex-shrink: 0;
  }
  .file-item {
    display: flex;
    flex-direction: column;
    width: 100%;
    padding: 8px 16px;
    border: none;
    background: transparent;
    cursor: pointer;
    text-align: left;
    gap: 2px;
  }
  .file-item:hover {
    background: var(--bg-elevated, #18181b);
  }
  .file-item.active {
    background: var(--bg-elevated, #18181b);
    border-left: 2px solid var(--accent, #3b82f6);
  }
  .file-name {
    font-size: 13px;
    font-weight: 500;
    color: var(--text-primary, #fafafa);
  }
  .file-source {
    font-size: 11px;
    color: var(--text-muted, #71717a);
  }
  .editor-area {
    flex: 1;
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .editor-toolbar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 8px 16px;
    border-bottom: 1px solid var(--border, #27272a);
    background: var(--bg-card, #111113);
  }
  .active-filename {
    font-size: 13px;
    font-weight: 500;
    color: var(--text-primary, #fafafa);
    font-family: monospace;
  }
  .toolbar-actions {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .save-msg {
    font-size: 12px;
    color: var(--success, #22c55e);
  }
  .advanced-hint,
  .read-only-note {
    margin: 0;
    padding: 8px 24px;
    font-size: 12px;
    color: var(--text-secondary, #a1a1aa);
  }
  .read-only-note {
    padding: 8px 16px;
    color: var(--warning, #eab308);
  }
  .btn {
    padding: 6px 14px;
    border: 1px solid var(--border, #27272a);
    border-radius: 6px;
    font-size: 13px;
    cursor: pointer;
    background: var(--bg-elevated, #18181b);
    color: var(--text-primary, #fafafa);
  }
  .btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .btn.primary {
    background: var(--accent, #3b82f6);
    border-color: var(--accent, #3b82f6);
    color: #fff;
  }
  .btn.secondary:hover:not(:disabled) {
    background: var(--bg-card, #111113);
  }
  .toml-editor {
    flex: 1;
    width: 100%;
    padding: 16px;
    border: none;
    background: var(--bg-base, #0a0a0b);
    color: var(--text-primary, #fafafa);
    font-family: 'SF Mono', 'Fira Code', 'Cascadia Code', monospace;
    font-size: 13px;
    line-height: 1.6;
    resize: none;
    outline: none;
    tab-size: 2;
  }
  .toml-editor.readonly {
    color: var(--text-secondary, #a1a1aa);
    cursor: default;
  }
  .merged-view {
    flex: 1;
    display: flex;
    min-height: 0;
  }
</style>
