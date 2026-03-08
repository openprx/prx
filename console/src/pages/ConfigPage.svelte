<script>
  import { SCHEMA, SCHEMA_HANDLED_KEYS, buildConfigNavGroups, configSectionId, focusConfigSection, humanizeKey } from '../lib/config-nav';
  import { configStore, loadConfigStore, updateConfigStore } from '../lib/config-store.svelte.js';
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';
  import { highlightJson } from '../lib/jsonHighlight';
  import {
    Zap, Globe, MessageSquare, Bot, Brain, Shield, HeartPulse,
    RefreshCw, Clock, GitBranch, BarChart3, Search, DollarSign,
    Settings, Cable, BadgeCheck, Database, Code2
  } from '@lucide/svelte';

  // ── State ──────────────────────────────────────────────────────
  let config = $state(null);
  let originalConfig = $state(null);
  let status = $state(null);
  let loading = $state(true);
  let saving = $state(false);
  let errorMessage = $state('');
  let saveMessage = $state('');
  let saveMessageTone = $state('success');
  let showRawJson = $state(false);
  let showDiff = $state(false);
  let revealedFields = $state(new Set());
  let activeNavGroup = $state('provider');

  // ── Icon map ───────────────────────────────────────────────────
  const ICON_MAP = {
    provider: Zap,
    gateway: Globe,
    channels: MessageSquare,
    agent: Bot,
    memory: Brain,
    security: Shield,
    heartbeat: HeartPulse,
    reliability: RefreshCw,
    scheduler: Clock,
    sessions_spawn: GitBranch,
    observability: BarChart3,
    web_search: Search,
    cost: DollarSign,
    runtime: Settings,
    tunnel: Cable,
    identity: BadgeCheck,
  };

  // ── Type helpers ───────────────────────────────────────────────

  function isPlainObj(v) {
    return v !== null && typeof v === 'object' && !Array.isArray(v);
  }

  function inferFieldType(value) {
    if (typeof value === 'boolean') return 'bool';
    if (typeof value === 'number') return 'number';
    if (Array.isArray(value)) return 'array';
    if (isPlainObj(value)) return 'object';
    return 'string';
  }

  function isSensitiveKey(key) {
    const lower = String(key).toLowerCase();
    return ['key', 'token', 'secret', 'password', 'auth', 'credential', 'private'].some(s => lower.includes(s));
  }

  // ── Schema helpers ─────────────────────────────────────────────

  /**
   * For a given SCHEMA group, find sub-keys in the actual config
   * that are NOT explicitly listed in group.fields.
   * e.g. channels group only lists channels_config.message_timeout_secs and channels_config.cli
   * but config.channels_config may also have .signal, .wacli, etc.
   */
  function getGroupExtraFields(group) {
    if (!config) return [];
    const handledPaths = new Set(Object.keys(group.fields));
    // Find which top-level config keys this group touches
    const topKeys = new Set();
    for (const fp of Object.keys(group.fields)) {
      topKeys.add(fp.split('.')[0]);
    }
    const extras = [];
    for (const topKey of topKeys) {
      const topVal = config[topKey];
      if (isPlainObj(topVal)) {
        for (const [k, v] of Object.entries(topVal)) {
          const path = `${topKey}.${k}`;
          if (!handledPaths.has(path)) {
            extras.push({ path, key: k, value: v });
          }
        }
      }
    }
    return extras;
  }

  // ── Dynamic groups ─────────────────────────────────────────────

  /** Top-level config keys not covered by any SCHEMA group */
  const dynamicGroups = $derived((() => {
    if (!config) return [];
    return Object.keys(config)
      .filter(k => !SCHEMA_HANDLED_KEYS.has(k))
      .sort();
  })());

  const schemaGroups = Object.entries(SCHEMA);

  const navGroups = $derived(buildConfigNavGroups(config));

  // ── Nested value access ────────────────────────────────────────

  function getNestedValue(obj, path) {
    if (!obj) return undefined;
    const parts = path.split('.');
    let current = obj;
    for (const part of parts) {
      if (current == null || typeof current !== 'object') return undefined;
      current = current[part];
    }
    return current;
  }

  function setNestedValue(obj, path, value) {
    const parts = path.split('.');
    let current = obj;
    for (let i = 0; i < parts.length - 1; i++) {
      if (current[parts[i]] == null || typeof current[parts[i]] !== 'object') {
        current[parts[i]] = {};
      }
      current = current[parts[i]];
    }
    current[parts[parts.length - 1]] = value;
  }

  function getFieldValue(fieldPath) {
    if (!config) return undefined;
    return getNestedValue(config, fieldPath);
  }

  function deepClone(obj) {
    return JSON.parse(JSON.stringify(obj));
  }

  function valuesEqual(a, b) {
    return JSON.stringify(a) === JSON.stringify(b);
  }

  // ── Change tracking (deep diff across all config) ─────────────

  /**
   * Recursively collect all differing leaf paths between two config objects.
   * Arrays are treated as leaf values (not recursed into).
   */
  function collectDiffs(newObj, oldObj, prefix) {
    const changes = [];
    const allKeys = new Set([
      ...Object.keys(newObj || {}),
      ...Object.keys(oldObj || {})
    ]);
    for (const key of allKeys) {
      const path = prefix ? `${prefix}.${key}` : key;
      const newVal = (newObj || {})[key];
      const oldVal = (oldObj || {})[key];
      if (isPlainObj(newVal) && isPlainObj(oldVal)) {
        changes.push(...collectDiffs(newVal, oldVal, path));
      } else if (!valuesEqual(newVal, oldVal)) {
        changes.push({ fieldPath: path, newVal, oldVal });
      }
    }
    return changes;
  }

  function getChangedFields() {
    if (!config || !originalConfig) return [];
    const diffs = collectDiffs(config, originalConfig, '');
    return diffs.map(d => {
      // Try to find a nice label from SCHEMA
      for (const group of Object.values(SCHEMA)) {
        if (group.fields[d.fieldPath]) {
          return { ...d, label: group.fields[d.fieldPath].label, group: group.label };
        }
      }
      const parts = d.fieldPath.split('.');
      return {
        ...d,
        label: humanizeKey(parts[parts.length - 1]),
        group: humanizeKey(parts[0])
      };
    });
  }

  const hasChanges = $derived(
    !!(config && originalConfig && JSON.stringify(config) !== JSON.stringify(originalConfig))
  );

  const changedFields = $derived(getChangedFields());
  const changedFieldPaths = $derived(new Set(changedFields.map(c => c.fieldPath)));

  /** Check if a path or any of its children have changes */
  function pathHasChanges(prefix) {
    for (const fp of changedFieldPaths) {
      if (fp === prefix || fp.startsWith(prefix + '.')) return true;
    }
    return false;
  }

  function focusGroup(groupKey) {
    activeNavGroup = groupKey;
    focusConfigSection(groupKey);
  }

  function focusHashTarget() {
    if (typeof window === 'undefined') return;
    const hash = window.location.hash.replace(/^#/, '');
    if (!hash.startsWith('config-section-')) return;
    const groupKey = hash.replace(/^config-section-/, '');
    if (!navGroups.some(group => group.groupKey === groupKey)) return;
    focusGroup(groupKey);
  }

  const prettyConfig = $derived(config ? JSON.stringify(config, null, 2) : '');
  const highlightedConfig = $derived(highlightJson(prettyConfig));

  // ── Field update ───────────────────────────────────────────────

  function updateField(fieldPath, value) {
    if (!config) return;
    const newConfig = deepClone(config);
    setNestedValue(newConfig, fieldPath, value);
    config = newConfig;
  }

  // ── Array field helpers (for SCHEMA array fields) ─────────────

  function addArrayItem(fieldPath) {
    const arr = getFieldValue(fieldPath);
    const newArr = Array.isArray(arr) ? [...arr, ''] : [''];
    updateField(fieldPath, newArr);
  }

  function removeArrayItem(fieldPath, index) {
    const arr = getFieldValue(fieldPath);
    if (!Array.isArray(arr)) return;
    updateField(fieldPath, arr.filter((_, i) => i !== index));
  }

  function updateArrayItem(fieldPath, index, value) {
    const arr = getFieldValue(fieldPath);
    if (!Array.isArray(arr)) return;
    const newArr = [...arr];
    newArr[index] = value;
    updateField(fieldPath, newArr);
  }

  // ── Sensitive field toggle ─────────────────────────────────────

  function toggleReveal(fieldPath) {
    const newSet = new Set(revealedFields);
    if (newSet.has(fieldPath)) {
      newSet.delete(fieldPath);
    } else {
      newSet.add(fieldPath);
    }
    revealedFields = newSet;
  }

  // ── Format display value ──────────────────────────────────────

  function formatValue(val) {
    if (val === null || val === undefined) return 'null';
    if (typeof val === 'boolean') return val ? 'true' : 'false';
    if (Array.isArray(val)) return JSON.stringify(val);
    if (typeof val === 'object') return JSON.stringify(val);
    return String(val);
  }

  // ── Determine if a dynamic value needs a JSON editor ──────────

  function needsJsonEditor(value) {
    if (isPlainObj(value)) return true;
    if (Array.isArray(value) && value.some(item => isPlainObj(item) || Array.isArray(item))) return true;
    return false;
  }

  // ── Load / Save ───────────────────────────────────────────────

  async function loadConfig() {
    try {
      await loadConfigStore();
      config = typeof configStore.data === 'object' && configStore.data ? deepClone(configStore.data) : {};
      originalConfig = deepClone(config);
      status = configStore.status;
      errorMessage = '';
    } catch (error) {
      errorMessage = error instanceof Error ? error.message : t('config.loadFailed');
    } finally {
      loading = false;
    }
  }

  async function saveConfig() {
    if (!hasChanges || saving) return;
    saving = true;
    saveMessage = '';
    saveMessageTone = 'success';
    try {
      const partial = {};
      for (const change of changedFields) {
        setNestedValue(partial, change.fieldPath, change.newVal);
      }
      const result = await api.saveConfig(partial);
      originalConfig = deepClone(config);
      updateConfigStore(deepClone(config));
      showDiff = false;
      if (result?.restart_required) {
        saveMessage = t('config.saveRestartRequired');
      } else {
        saveMessage = t('config.saveSuccess');
      }
      setTimeout(() => { saveMessage = ''; }, 5000);
    } catch (error) {
      saveMessageTone = 'error';
      saveMessage = t('config.saveFailed', {
        message: error instanceof Error ? error.message : String(error)
      });
    } finally {
      saving = false;
    }
  }

  function discardChanges() {
    if (!originalConfig) return;
    config = deepClone(originalConfig);
    showDiff = false;
  }

  async function copyToClipboard() {
    if (!prettyConfig || typeof navigator === 'undefined' || !navigator.clipboard) return;
    try {
      await navigator.clipboard.writeText(prettyConfig);
    } catch {}
  }

  $effect(() => { loadConfig(); });

  $effect(() => {
    if (loading || showRawJson || navGroups.length === 0) return;
    queueMicrotask(() => {
      focusHashTarget();
    });
  });
</script>

<!--
  ── Snippets for dynamic field rendering ─────────────────────────
  These must appear before the section that uses them.
-->

{#snippet schemaField(fieldPath, fieldDef)}
  {@const currentValue = getFieldValue(fieldPath)}
  {@const isChanged = changedFieldPaths.has(fieldPath)}
  {@const isRevealed = revealedFields.has(fieldPath)}

  <div class="rounded-lg border p-3 transition-colors {isChanged ? 'border-sky-500/50 bg-sky-500/5' : 'border-gray-200 bg-gray-50/40 dark:border-gray-700 dark:bg-gray-900/40'}">
    <div class="flex items-start justify-between gap-3">
      <div class="flex-1 min-w-0">
        <label class="block text-sm font-medium text-gray-700 dark:text-gray-200">
          {fieldDef.label}
          {#if isChanged}
            <span class="ml-1.5 text-xs text-sky-500 dark:text-sky-400">已修改</span>
          {/if}
        </label>
        <p class="mt-0.5 text-xs text-gray-400 dark:text-gray-500">{fieldDef.desc}</p>
      </div>

      <div class="flex-shrink-0 w-64">
        {#if fieldDef.type === 'bool'}
          <button
            type="button"
            onclick={() => updateField(fieldPath, !currentValue)}
            class="relative inline-flex h-6 w-11 items-center rounded-full transition-colors {currentValue ? 'bg-sky-600' : 'bg-gray-400 dark:bg-gray-600'}"
          >
            <span class="inline-block h-4 w-4 transform rounded-full bg-white transition-transform {currentValue ? 'translate-x-6' : 'translate-x-1'}"></span>
          </button>

        {:else if fieldDef.type === 'enum'}
          <select
            value={currentValue ?? fieldDef.default}
            onchange={(e) => updateField(fieldPath, e.target.value)}
            class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
          >
            {#each fieldDef.options as option}
              <option value={option}>{option || '(默认)'}</option>
            {/each}
          </select>

        {:else if fieldDef.type === 'number'}
          <input
            type="number"
            value={currentValue ?? fieldDef.default}
            min={fieldDef.min}
            max={fieldDef.max}
            step={fieldDef.step ?? 1}
            oninput={(e) => {
              const v = fieldDef.step && fieldDef.step < 1
                ? parseFloat(e.target.value)
                : parseInt(e.target.value, 10);
              if (!isNaN(v)) updateField(fieldPath, v);
            }}
            class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
            placeholder={String(fieldDef.default)}
          />

        {:else if fieldDef.type === 'array'}
          <div class="space-y-1.5">
            {#if Array.isArray(currentValue)}
              {#each currentValue as item, i}
                <div class="flex gap-1">
                  <input
                    type="text"
                    value={item}
                    oninput={(e) => updateArrayItem(fieldPath, i, e.target.value)}
                    class="flex-1 rounded border border-gray-300 bg-white px-2 py-1 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
                  />
                  <button
                    type="button"
                    onclick={() => removeArrayItem(fieldPath, i)}
                    class="rounded border border-gray-300 bg-white px-2 py-1 text-xs text-red-500 hover:bg-red-500/10 dark:border-gray-600 dark:bg-gray-800 dark:text-red-400"
                  >×</button>
                </div>
              {/each}
            {/if}
            <button
              type="button"
              onclick={() => addArrayItem(fieldPath)}
              class="rounded border border-dashed border-gray-300 px-2 py-1 text-xs text-gray-500 hover:border-sky-500 hover:text-sky-500 dark:border-gray-600 dark:text-gray-400 dark:hover:border-sky-500 dark:hover:text-sky-400"
            >+ 添加</button>
          </div>

        {:else if fieldDef.sensitive}
          <div class="flex gap-1">
            <input
              type={isRevealed ? 'text' : 'password'}
              value={currentValue ?? ''}
              oninput={(e) => updateField(fieldPath, e.target.value)}
              class="flex-1 rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
              placeholder={fieldDef.default || '未设置'}
            />
            <button
              type="button"
              onclick={() => toggleReveal(fieldPath)}
              class="rounded-lg border border-gray-300 bg-white px-2 text-xs text-gray-500 hover:text-gray-700 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-400 dark:hover:text-gray-200"
            >
              {isRevealed ? '隐藏' : '显示'}
            </button>
          </div>

        {:else}
          <input
            type="text"
            value={currentValue ?? ''}
            oninput={(e) => updateField(fieldPath, e.target.value)}
            class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
            placeholder={fieldDef.default || '未设置'}
          />
        {/if}
      </div>
    </div>
  </div>
{/snippet}

{#snippet dynLeafInput(path, currentVal)}
  {@const isSens = isSensitiveKey(path.split('.').pop() ?? '')}
  {@const revealed = revealedFields.has(path)}

  {#if typeof currentVal === 'boolean'}
    <button
      type="button"
      onclick={() => updateField(path, !currentVal)}
      class="relative inline-flex h-6 w-11 items-center rounded-full transition-colors {currentVal ? 'bg-sky-600' : 'bg-gray-400 dark:bg-gray-600'}"
    >
      <span class="inline-block h-4 w-4 transform rounded-full bg-white transition-transform {currentVal ? 'translate-x-6' : 'translate-x-1'}"></span>
    </button>

  {:else if typeof currentVal === 'number'}
    <input
      type="number"
      value={currentVal}
      oninput={(e) => { const v = parseFloat(e.target.value); if (!isNaN(v)) updateField(path, v); }}
      class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
    />

  {:else if Array.isArray(currentVal)}
    <!-- Primitive array inline editor -->
    <div class="space-y-1.5">
      {#each currentVal as item, i}
        <div class="flex gap-1">
          <input
            type="text"
            value={item}
            oninput={(e) => {
              const arr = [...(getNestedValue(config, path) || [])];
              arr[i] = e.target.value;
              updateField(path, arr);
            }}
            class="flex-1 rounded border border-gray-300 bg-white px-2 py-1 text-sm focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
          />
          <button
            type="button"
            onclick={() => {
              const arr = (getNestedValue(config, path) || []).filter((_, j) => j !== i);
              updateField(path, arr);
            }}
            class="rounded border border-gray-300 px-2 py-1 text-xs text-red-500 hover:bg-red-500/10 dark:border-gray-600 dark:bg-gray-800"
          >×</button>
        </div>
      {/each}
      <button
        type="button"
        onclick={() => { const arr = [...(getNestedValue(config, path) || []), '']; updateField(path, arr); }}
        class="rounded border border-dashed border-gray-300 px-2 py-1 text-xs text-gray-500 hover:border-sky-500 hover:text-sky-500 dark:border-gray-600"
      >+ 添加</button>
    </div>

  {:else if isSens}
    <div class="flex gap-1">
      <input
        type={revealed ? 'text' : 'password'}
        value={currentVal ?? ''}
        oninput={(e) => updateField(path, e.target.value)}
        class="flex-1 rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
        placeholder="未设置"
      />
      <button
        type="button"
        onclick={() => toggleReveal(path)}
        class="rounded-lg border border-gray-300 bg-white px-2 text-xs text-gray-500 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-400"
      >
        {revealed ? '隐藏' : '显示'}
      </button>
    </div>

  {:else}
    <input
      type="text"
      value={currentVal ?? ''}
      oninput={(e) => updateField(path, e.target.value)}
      class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"
      placeholder="未设置"
    />
  {/if}
{/snippet}

{#snippet dynJsonEditor(path, currentVal)}
  {@const jsonStr = JSON.stringify(currentVal, null, 2)}
  {@const lineCount = Math.min(15, (jsonStr.match(/\n/g) || []).length + 2)}
  <textarea
    value={jsonStr}
    rows={lineCount}
    class="w-full rounded-lg border border-gray-300 bg-white font-mono text-xs leading-relaxed p-2 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200 resize-y"
    onblur={(e) => {
      try {
        const parsed = JSON.parse(e.target.value);
        updateField(path, parsed);
      } catch {
        // Reset to last valid value
        e.target.value = JSON.stringify(getNestedValue(config, path) ?? currentVal, null, 2);
      }
    }}
  ></textarea>
{/snippet}

<!--
  dynFieldRow: renders a single field row for dynamic sections.
  path: dot-path like "agents.glm5.provider"
  label: display key name
  value: the ORIGINAL value (for type detection)
-->
{#snippet dynFieldRow(path, label, value)}
  {@const currentVal = getNestedValue(config, path) ?? value}
  {@const isChanged = changedFieldPaths.has(path)}

  <div class="rounded-lg border p-3 transition-colors {isChanged ? 'border-sky-500/50 bg-sky-500/5' : 'border-gray-200 bg-gray-50/40 dark:border-gray-700 dark:bg-gray-900/40'}">
    {#if needsJsonEditor(currentVal)}
      <!-- Complex value: label on top, JSON editor below -->
      <div class="mb-2 flex items-center gap-2">
        <Code2 size={13} class="flex-shrink-0 text-gray-400" />
        <span class="font-mono text-xs font-medium text-gray-600 dark:text-gray-300">{label}</span>
        {#if isChanged}<span class="text-xs text-sky-500">已修改</span>{/if}
      </div>
      {@render dynJsonEditor(path, currentVal)}
    {:else}
      <!-- Primitive: label + input side by side -->
      <div class="flex items-center justify-between gap-3">
        <span class="min-w-0 flex-1 font-mono text-sm text-gray-700 dark:text-gray-200">
          {label}
          {#if isChanged}<span class="ml-1.5 text-xs text-sky-500">已修改</span>{/if}
        </span>
        <div class="w-56 flex-shrink-0">
          {@render dynLeafInput(path, currentVal)}
        </div>
      </div>
    {/if}
  </div>
{/snippet}

<!--
  dynSubSection: renders a named sub-object (depth 1 in a dynamic group),
  e.g. agents.glm5 → shows its leaf fields individually.
-->
{#snippet dynSubSection(path, label, value)}
  {@const isObj = isPlainObj(value)}
  {@const sectionChanged = pathHasChanges(path)}

  <details class="rounded-lg border border-gray-200 dark:border-gray-700">
    <summary class="cursor-pointer select-none flex items-center gap-2 px-3 py-2 text-sm font-medium text-gray-700 dark:text-gray-200 hover:bg-gray-50 dark:hover:bg-gray-700/50 rounded-lg">
      <span class="font-mono">{label}</span>
      {#if sectionChanged}
        <span class="inline-flex h-2 w-2 rounded-full bg-sky-500"></span>
      {/if}
      {#if !isObj}
        <span class="ml-auto text-xs text-gray-400">{inferFieldType(value)}</span>
      {/if}
    </summary>
    <div class="border-t border-gray-200 px-3 py-2 space-y-2 dark:border-gray-700">
      {#if isObj}
        {#each Object.entries(value) as [k, v]}
          {@const subPath = `${path}.${k}`}
          {#if isPlainObj(v)}
            <!-- Nested object: show as JSON editor with a label -->
            {@render dynFieldRow(subPath, k, v)}
          {:else}
            {@render dynFieldRow(subPath, k, v)}
          {/if}
        {/each}
      {:else}
        {@render dynFieldRow(path, label, value)}
      {/if}
    </div>
  </details>
{/snippet}

<!--
  dynGroupContent: renders the content of a dynamic top-level group.
  For objects: iterate sub-keys, showing each as dynSubSection or dynFieldRow.
  For arrays/primitives: render directly.
-->
{#snippet dynGroupContent(topKey, value)}
  {#if isPlainObj(value)}
    <div class="space-y-2">
      {#each Object.entries(value) as [k, v]}
        {#if isPlainObj(v)}
          <!-- Map-of-configs pattern (agents.*, mcp.servers.*, nodes.*) -->
          {@render dynSubSection(`${topKey}.${k}`, k, v)}
        {:else}
          <!-- Direct sub-field -->
          {@render dynFieldRow(`${topKey}.${k}`, k, v)}
        {/if}
      {/each}
    </div>
  {:else if Array.isArray(value)}
    {@render dynFieldRow(topKey, topKey, value)}
  {:else}
    {@render dynFieldRow(topKey, topKey, value)}
  {/if}
{/snippet}

<!-- ─────────────────────────────────────────────────────────────── -->

<section class="space-y-4 pb-24">
  <!-- Header -->
  <div class="flex items-center justify-between gap-4">
    <h2 class="text-2xl font-semibold">{t('config.title')}</h2>
    <div class="flex items-center gap-2">
      <button
        type="button"
        onclick={() => (showRawJson = !showRawJson)}
        class="rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-600 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700"
      >
        {showRawJson ? '结构化编辑' : 'JSON 视图'}
      </button>
      <button
        type="button"
        onclick={copyToClipboard}
        class="rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-600 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700"
      >
        复制 JSON
      </button>
    </div>
  </div>

  {#if loading}
    <p class="text-sm text-gray-500 dark:text-gray-400">加载配置中...</p>
  {:else if errorMessage}
    <p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300">
      {errorMessage}
    </p>
  {:else if showRawJson}
    <div class="overflow-x-auto rounded-xl border border-gray-200 bg-gray-50 p-4 dark:border-gray-700 dark:bg-gray-950">
      <pre class="text-sm leading-6 text-gray-700 dark:text-gray-200"><code>{@html highlightedConfig}</code></pre>
    </div>
  {:else}
    <div class="space-y-3">
      <div class="sticky top-0 z-20 -mx-1 overflow-x-auto rounded-xl border border-gray-200 bg-white/95 px-3 py-3 backdrop-blur dark:border-gray-700 dark:bg-gray-900/95">
        <div class="flex min-w-max items-center gap-2">
          {#each navGroups as nav}
            {@const navChanged = pathHasChanges(nav.groupKey)}
            <button
              type="button"
              onclick={() => focusGroup(nav.groupKey)}
              class="inline-flex items-center gap-2 rounded-full border px-3 py-1.5 text-sm transition {activeNavGroup === nav.groupKey ? 'border-sky-500 bg-sky-500/10 text-sky-700 dark:text-sky-300' : 'border-gray-300 bg-white text-gray-600 hover:border-sky-400 hover:text-sky-600 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:border-sky-500 dark:hover:text-sky-300'}"
            >
              <span>{nav.label}</span>
              {#if nav.dynamic}
                <span class="rounded-full bg-gray-100 px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-wide text-gray-500 dark:bg-gray-700 dark:text-gray-300">Auto</span>
              {/if}
              {#if navChanged}
                <span class="inline-flex h-2 w-2 rounded-full bg-sky-500"></span>
              {/if}
            </button>
          {/each}
        </div>
      </div>

      <!-- ── SCHEMA groups ───────────────────────────────────────── -->
      {#each schemaGroups as [groupKey, group]}
        {@const IconComponent = ICON_MAP[groupKey]}
        {@const extraFields = getGroupExtraFields(group)}
        {@const schemaFieldPaths = Object.keys(group.fields)}
        {@const groupHasChanges = schemaFieldPaths.some(fp => changedFieldPaths.has(fp)) || extraFields.some(ef => pathHasChanges(ef.path))}

        <details
          id={configSectionId(groupKey)}
          class="group scroll-mt-24 rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"
          open={group.defaultOpen}
          ontoggle={(event) => {
            if (event.currentTarget.open) {
              activeNavGroup = groupKey;
            }
          }}
        >
          <summary class="cursor-pointer select-none px-4 py-3 text-base font-semibold text-gray-900 flex items-center gap-2 dark:text-gray-100">
            {#if IconComponent}
              <IconComponent size={18} class="text-gray-500 dark:text-gray-400" />
            {/if}
            <span>{group.label}</span>
            {#if groupHasChanges}
              <span class="ml-2 inline-flex h-2 w-2 rounded-full bg-sky-500"></span>
            {/if}
          </summary>

          <div class="border-t border-gray-200 px-4 py-3 space-y-3 dark:border-gray-700">
            <!-- Known schema fields -->
            {#each Object.entries(group.fields) as [fieldPath, fieldDef]}
              {@render schemaField(fieldPath, fieldDef)}
            {/each}

            <!-- Extra sub-fields not defined in this SCHEMA group -->
            {#if extraFields.length > 0}
              <div class="mt-2 border-t border-gray-100 pt-3 dark:border-gray-700/60">
                <p class="mb-2 text-xs font-medium uppercase tracking-wider text-gray-400 dark:text-gray-500">其他子配置</p>
                <div class="space-y-2">
                  {#each extraFields as { path, key, value }}
                    {#if isPlainObj(value)}
                      {@render dynSubSection(path, key, value)}
                    {:else}
                      {@render dynFieldRow(path, key, value)}
                    {/if}
                  {/each}
                </div>
              </div>
            {/if}
          </div>
        </details>
      {/each}

      <!-- ── Dynamic groups (top-level keys not in SCHEMA) ────────── -->
      {#if dynamicGroups.length > 0}
        <div class="pt-1">
          <p class="mb-2 px-1 text-xs font-semibold uppercase tracking-wider text-gray-400 dark:text-gray-500">
            自动发现的配置项
          </p>
          <div class="space-y-3">
            {#each dynamicGroups as groupKey}
              {@const groupValue = config[groupKey]}
              {@const groupChanged = pathHasChanges(groupKey)}
              {@const typeLabel = inferFieldType(groupValue)}

              <details
                id={configSectionId(groupKey)}
                class="scroll-mt-24 rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"
                ontoggle={(event) => {
                  if (event.currentTarget.open) {
                    activeNavGroup = groupKey;
                  }
                }}
              >
                <summary class="cursor-pointer select-none px-4 py-3 flex items-center gap-2 dark:text-gray-100">
                  <Database size={18} class="flex-shrink-0 text-gray-400 dark:text-gray-500" />
                  <span class="font-mono text-sm font-semibold text-gray-800 dark:text-gray-100">{groupKey}</span>
                  {#if groupChanged}
                    <span class="inline-flex h-2 w-2 rounded-full bg-sky-500"></span>
                  {/if}
                  <span class="ml-auto text-xs text-gray-400 dark:text-gray-500">{typeLabel}</span>
                </summary>
                <div class="border-t border-gray-200 px-4 py-3 dark:border-gray-700">
                  {@render dynGroupContent(groupKey, groupValue)}
                </div>
              </details>
            {/each}
          </div>
        </div>
      {/if}

    </div>
  {/if}

  <!-- ── Save bar (fixed at bottom) ────────────────────────────── -->
  {#if hasChanges && !loading && !showRawJson}
    <div class="fixed bottom-0 left-0 right-0 z-50 border-t border-gray-200 bg-white/95 px-6 py-3 backdrop-blur-sm dark:border-gray-700 dark:bg-gray-900/95">
      <div class="mx-auto flex max-w-5xl items-center justify-between gap-4">
        <div class="flex items-center gap-3">
          <span class="text-sm text-sky-600 dark:text-sky-400">{changedFields.length} 项更改</span>
          <button
            type="button"
            onclick={() => (showDiff = !showDiff)}
            class="text-sm text-gray-500 underline hover:text-gray-700 dark:text-gray-400 dark:hover:text-gray-200"
          >
            {showDiff ? '隐藏详情' : '查看详情'}
          </button>
        </div>
        <div class="flex items-center gap-2">
          <button
            type="button"
            onclick={discardChanges}
            class="rounded-lg border border-gray-300 bg-white px-4 py-2 text-sm text-gray-600 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700"
          >
            放弃修改
          </button>
          <button
            type="button"
            onclick={saveConfig}
            disabled={saving}
            class="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"
          >
            {saving ? '保存中...' : '保存配置'}
          </button>
        </div>
      </div>

      <!-- Diff panel -->
      {#if showDiff}
        <div class="mx-auto mt-3 max-w-5xl rounded-lg border border-gray-200 bg-gray-50 p-3 dark:border-gray-700 dark:bg-gray-950">
          <p class="mb-2 text-xs font-medium text-gray-500 dark:text-gray-400">变更详情</p>
          <div class="space-y-1.5 max-h-48 overflow-y-auto">
            {#each changedFields as change}
              <div class="flex items-start gap-2 text-xs flex-wrap">
                <span class="flex-shrink-0 text-gray-400 dark:text-gray-500">{change.group}</span>
                <span class="font-medium text-gray-600 dark:text-gray-300">{change.label}</span>
                <span class="text-red-500 line-through dark:text-red-400 break-all">{formatValue(change.oldVal)}</span>
                <span class="text-gray-400 dark:text-gray-600">→</span>
                <span class="text-green-600 dark:text-green-400 break-all">{formatValue(change.newVal)}</span>
              </div>
            {/each}
          </div>
        </div>
      {/if}
    </div>
  {/if}

  <!-- Save message toast -->
  {#if saveMessage}
    <div class="fixed bottom-20 left-1/2 z-50 -translate-x-1/2 rounded-lg border px-4 py-2 text-sm shadow-lg {saveMessageTone === 'error' ? 'border-red-500/30 bg-red-500/10 text-red-600 dark:text-red-300' : 'border-green-500/30 bg-green-500/10 text-green-700 dark:text-green-300'}">
      {saveMessage}
    </div>
  {/if}
</section>
