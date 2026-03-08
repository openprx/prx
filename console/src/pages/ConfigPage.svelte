<script>
  import { t } from '../lib/i18n/index.svelte.js';
  import { api } from '../lib/api.js';

  let files = $state([]);
  let activeFile = $state('');
  let editContent = $state('');
  let originalContent = $state('');
  let loading = $state(true);
  let saving = $state(false);
  let error = $state('');
  let saveMsg = $state('');
  let mergedJson = $state('');
  let viewMode = $state('files'); // 'files' | 'merged'

  const hasChanges = $derived(editContent !== originalContent);

  async function loadFiles() {
    loading = true;
    error = '';
    try {
      const result = await api.getConfigFiles();
      files = Array.isArray(result) ? result : (result?.files ?? []);
      if (files.length > 0 && !activeFile) {
        activeFile = files[0].filename;
        editContent = files[0].content;
        originalContent = files[0].content;
      }
    } catch (e) {
      error = e.message || 'Failed to load config files';
    }
    loading = false;
  }

  async function loadMerged() {
    try {
      const config = await api.getConfig();
      mergedJson = JSON.stringify(config, null, 2);
    } catch (e) {
      mergedJson = `Error: ${e.message}`;
    }
  }

  function selectFile(filename) {
    const file = files.find(f => f.filename === filename);
    if (!file) return;
    activeFile = filename;
    editContent = file.content;
    originalContent = file.content;
    saveMsg = '';
  }

  async function saveFile() {
    if (!activeFile || !hasChanges) return;
    saving = true;
    saveMsg = '';
    try {
      await api.saveConfigFile(activeFile, editContent);
      originalContent = editContent;
      const idx = files.findIndex(f => f.filename === activeFile);
      if (idx >= 0) files[idx] = { ...files[idx], content: editContent };
      saveMsg = 'Saved';
      setTimeout(() => saveMsg = '', 2000);
    } catch (e) {
      saveMsg = `Error: ${e.message}`;
    }
    saving = false;
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
    <h2>{t('config.title') || 'Configuration'}</h2>
    <div class="view-tabs">
      <button
        class="tab-btn" class:active={viewMode === 'files'}
        onclick={() => { viewMode = 'files'; }}
      >{t('config.files') || 'Files'}</button>
      <button
        class="tab-btn" class:active={viewMode === 'merged'}
        onclick={() => { viewMode = 'merged'; loadMerged(); }}
      >{t('config.merged') || 'Merged View'}</button>
    </div>
  </div>

  {#if loading}
    <div class="state-msg">{t('common.loading') || 'Loading...'}</div>
  {:else if error}
    <div class="state-msg error">{error}</div>
  {:else if viewMode === 'files'}
    <div class="editor-layout">
      <div class="file-list">
        {#each files as file}
          <button
            class="file-item" class:active={activeFile === file.filename}
            onclick={() => selectFile(file.filename)}
          >
            <span class="file-name">{file.filename}</span>
            <span class="file-source">{file.source}</span>
          </button>
        {/each}
      </div>
      <div class="editor-area">
        <div class="editor-toolbar">
          <span class="active-filename">{activeFile}</span>
          <div class="toolbar-actions">
            {#if saveMsg}
              <span class="save-msg" class:error={saveMsg.startsWith('Error')}>{saveMsg}</span>
            {/if}
            <button class="btn secondary" onclick={revert} disabled={!hasChanges}>
              {t('config.revert') || 'Revert'}
            </button>
            <button class="btn primary" onclick={saveFile} disabled={!hasChanges || saving}>
              {saving ? (t('common.saving') || 'Saving...') : (t('config.save') || 'Save')}
            </button>
          </div>
        </div>
        <textarea
          class="toml-editor"
          bind:value={editContent}
          spellcheck="false"
        ></textarea>
      </div>
    </div>
  {:else}
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
  .config-header h2 {
    margin: 0;
    font-size: 18px;
    font-weight: 600;
    color: var(--text-primary, #fafafa);
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
  .state-msg {
    padding: 40px;
    text-align: center;
    color: var(--text-secondary, #a1a1aa);
  }
  .state-msg.error {
    color: var(--error, #ef4444);
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
  .save-msg.error {
    color: var(--error, #ef4444);
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
