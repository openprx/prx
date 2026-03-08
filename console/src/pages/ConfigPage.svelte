<script>
  import { slide, fade } from 'svelte/transition';
  import {
    BadgeCheck,
    Bot,
    Brain,
    Cable,
    ChevronDown,
    Clock,
    Code2,
    Copy,
    Database,
    DollarSign,
    Eye,
    EyeOff,
    FileJson2,
    Files,
    Globe,
    GitBranch,
    HeartPulse,
    RefreshCw,
    RotateCcw,
    Save,
    Search,
    Settings,
    Shield,
    Zap,
    BarChart3,
    MessageSquare
  } from '@lucide/svelte';
  import {
    SCHEMA,
    buildConfigNavGroups,
    configSectionId,
    focusConfigSection,
    humanizeKey
  } from '../lib/config-nav';
  import { configStore, loadConfigStore, updateConfigStore } from '../lib/config-store.svelte.js';
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';

  const REDACTION_MASK = '***';
  const EMPTY_FILE_SAVE_STATE = Object.freeze({});

  let config = $state({});
  let originalConfig = $state({});
  let schemaDocument = $state(null);
  let configFiles = $state([]);
  let loading = $state(true);
  let errorMessage = $state('');
  let saveMessage = $state('');
  let saveMessageTone = $state('success');
  let savingConfig = $state(false);
  let advancedMode = $state(false);
  let searchQuery = $state('');
  let activeNavGroup = $state('provider');
  let openGroups = $state(new Set());
  let openObjects = $state(new Set());
  let revealedFields = $state(new Set());
  let rawJsonDraft = $state('');
  let rawJsonError = $state('');
  let rawJsonDirty = $state(false);
  let fileDrafts = $state({});
  let fileSaveStates = $state(EMPTY_FILE_SAVE_STATE);

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
    identity: BadgeCheck
  };

  function cloneValue(value) {
    if (value === undefined) return undefined;
    return JSON.parse(JSON.stringify(value));
  }

  function isPlainObject(value) {
    return value !== null && typeof value === 'object' && !Array.isArray(value);
  }

  function valuesEqual(left, right) {
    return JSON.stringify(left) === JSON.stringify(right);
  }

  function hasOwn(object, key) {
    return Object.prototype.hasOwnProperty.call(object ?? {}, key);
  }

  function getNestedValue(source, path) {
    if (!path) return source;
    const parts = path.split('.');
    let current = source;
    for (const part of parts) {
      if (!isPlainObject(current) && !Array.isArray(current)) return undefined;
      current = current?.[part];
      if (current === undefined) return undefined;
    }
    return current;
  }

  function setNestedValue(target, path, value) {
    const parts = path.split('.');
    let cursor = target;
    for (let index = 0; index < parts.length - 1; index += 1) {
      const part = parts[index];
      if (!isPlainObject(cursor[part])) {
        cursor[part] = {};
      }
      cursor = cursor[part];
    }
    cursor[parts[parts.length - 1]] = value;
  }

  function deleteNestedValue(target, path) {
    const parts = path.split('.');
    let cursor = target;
    for (let index = 0; index < parts.length - 1; index += 1) {
      cursor = cursor?.[parts[index]];
      if (!isPlainObject(cursor)) return;
    }

    if (!cursor) return;
    delete cursor[parts[parts.length - 1]];
  }

  function updateField(path, value) {
    const nextConfig = cloneValue(config) ?? {};
    setNestedValue(nextConfig, path, value);
    config = nextConfig;
  }

  function resetFieldToDefault(path, defaultValue) {
    if (defaultValue === undefined) return;
    updateField(path, cloneValue(defaultValue));
  }

  function discardStructuredChanges() {
    config = cloneValue(originalConfig) ?? {};
    rawJsonDirty = false;
    rawJsonError = '';
  }

  function toggleSetMember(currentSet, id) {
    const next = new Set(currentSet);
    if (next.has(id)) {
      next.delete(id);
    } else {
      next.add(id);
    }
    return next;
  }

  function toggleReveal(path) {
    revealedFields = toggleSetMember(revealedFields, path);
  }

  function toggleGroup(groupKey) {
    openGroups = toggleSetMember(openGroups, groupKey);
  }

  function toggleObject(path) {
    openObjects = toggleSetMember(openObjects, path);
  }

  function focusGroup(groupKey) {
    activeNavGroup = groupKey;
    if (!openGroups.has(groupKey)) {
      const next = new Set(openGroups);
      next.add(groupKey);
      openGroups = next;
    }
    focusConfigSection(groupKey);
  }

  function isSensitiveKey(key) {
    const lower = String(key).toLowerCase();
    return [
      'key',
      'token',
      'secret',
      'password',
      'auth',
      'credential',
      'private'
    ].some((fragment) => lower.includes(fragment));
  }

  function formatDefaultValue(value) {
    if (value === undefined) return 'No default';
    if (typeof value === 'string') return value.length > 0 ? value : '(empty)';
    return JSON.stringify(value);
  }

  function formatValue(value) {
    if (value === undefined) return '(unset)';
    if (value === null) return 'null';
    if (typeof value === 'string') return value.length > 0 ? value : '(empty)';
    return JSON.stringify(value);
  }

  function isSearchMatch(query, ...parts) {
    if (!query) return true;
    const haystack = parts
      .filter((part) => typeof part === 'string' && part.trim().length > 0)
      .join(' ')
      .toLowerCase();
    return haystack.includes(query);
  }

  function resolveJsonPointer(root, pointer) {
    if (!pointer.startsWith('#/')) return null;
    const segments = pointer
      .slice(2)
      .split('/')
      .map((part) => part.replaceAll('~1', '/').replaceAll('~0', '~'));
    let current = root;
    for (const segment of segments) {
      current = current?.[segment];
      if (current === undefined) return null;
    }
    return current;
  }

  function mergeSchemas(base, extension) {
    const merged = { ...base, ...extension };
    if (base?.properties || extension?.properties) {
      merged.properties = {
        ...(base?.properties ?? {}),
        ...(extension?.properties ?? {})
      };
    }
    if (base?.required || extension?.required) {
      merged.required = Array.from(new Set([...(base?.required ?? []), ...(extension?.required ?? [])]));
    }
    if (extension?.items !== undefined) {
      merged.items = extension.items;
    } else if (base?.items !== undefined) {
      merged.items = base.items;
    }
    return merged;
  }

  function resolveSchema(schema) {
    if (!schema || typeof schema !== 'object') return {};
    let current = schema;

    if (current.$ref) {
      const resolved = resolveJsonPointer(schemaDocument, current.$ref);
      if (resolved) {
        current = mergeSchemas(resolved, { ...current, $ref: undefined });
      }
    }

    if (Array.isArray(current.allOf) && current.allOf.length > 0) {
      let merged = { ...current, allOf: undefined };
      for (const part of current.allOf) {
        merged = mergeSchemas(merged, resolveSchema(part));
      }
      current = merged;
    }

    return current;
  }

  function getNonNullVariants(schema) {
    const resolved = resolveSchema(schema);
    const variants = [...(resolved.oneOf ?? []), ...(resolved.anyOf ?? [])];
    return variants
      .map((variant) => resolveSchema(variant))
      .filter((variant) => {
        if (variant.const === null) return false;
        if (variant.type === 'null') return false;
        if (Array.isArray(variant.type) && variant.type.length === 1 && variant.type[0] === 'null') {
          return false;
        }
        return true;
      });
  }

  function getSchemaType(schema, value) {
    const resolved = resolveSchema(schema);

    if (Array.isArray(resolved.type)) {
      const nonNullTypes = resolved.type.filter((type) => type !== 'null');
      if (nonNullTypes.length === 1) {
        return nonNullTypes[0];
      }
    }

    if (resolved.type) {
      return resolved.type;
    }

    if (resolved.properties || value && isPlainObject(value)) {
      return 'object';
    }

    if (resolved.items || Array.isArray(value)) {
      return 'array';
    }

    const variants = getNonNullVariants(resolved);
    if (variants.length === 1) {
      return getSchemaType(variants[0], value);
    }

    if (typeof value === 'boolean') return 'boolean';
    if (typeof value === 'number') return Number.isInteger(value) ? 'integer' : 'number';
    if (typeof value === 'string') return 'string';

    return null;
  }

  function getEnumOptions(schema) {
    const resolved = resolveSchema(schema);

    if (Array.isArray(resolved.enum) && resolved.enum.length > 0) {
      return resolved.enum.map((value) => ({
        label: value === null ? '(null)' : String(value),
        value
      }));
    }

    const variants = [...(resolved.oneOf ?? []), ...(resolved.anyOf ?? [])]
      .map((variant) => resolveSchema(variant))
      .filter((variant) => variant.const !== undefined);

    if (variants.length > 0) {
      return variants.map((variant) => ({
        label: variant.title ?? (variant.const === null ? '(null)' : String(variant.const)),
        value: variant.const
      }));
    }

    return [];
  }

  function inferSchemaFromValue(value) {
    if (typeof value === 'boolean') return { type: 'boolean' };
    if (typeof value === 'number') return { type: Number.isInteger(value) ? 'integer' : 'number' };
    if (typeof value === 'string') return { type: 'string' };
    if (Array.isArray(value)) {
      if (value.every((item) => typeof item === 'string')) {
        return { type: 'array', items: { type: 'string' }, default: [] };
      }
      return { type: 'array' };
    }
    if (isPlainObject(value)) {
      return {
        type: 'object',
        properties: Object.fromEntries(
          Object.entries(value).map(([key, entry]) => [key, inferSchemaFromValue(entry)])
        )
      };
    }
    return {};
  }

  function getInputKind(schema, value) {
    const resolved = resolveSchema(schema);
    const enumOptions = getEnumOptions(resolved);
    if (enumOptions.length > 0) return 'enum';

    const type = getSchemaType(resolved, value);
    if (type === 'boolean') return 'boolean';
    if (type === 'number' || type === 'integer') return 'number';
    if (type === 'string') return 'string';
    if (type === 'object') return 'object';
    if (type === 'array') {
      const itemType = getSchemaType(resolved.items, Array.isArray(value) ? value[0] : undefined);
      return itemType === 'string' ? 'string-array' : 'json';
    }

    const variants = getNonNullVariants(resolved);
    if (
      variants.length === 2 &&
      variants.some((variant) => getSchemaType(variant) === 'boolean') &&
      variants.some((variant) => getSchemaType(variant) === 'string')
    ) {
      return 'enum';
    }

    return 'json';
  }

  function buildObjectChildren(path, schema, value, depth, query) {
    const resolved = resolveSchema(schema);
    const propertyMap = resolved.properties ?? {};
    const schemaKeys = Object.keys(propertyMap);
    const valueKeys = isPlainObject(value) ? Object.keys(value) : [];
    const orderedKeys = [...schemaKeys, ...valueKeys.filter((key) => !schemaKeys.includes(key))];

    return orderedKeys.map((key) => {
      const childPath = path ? `${path}.${key}` : key;
      const childSchema =
        propertyMap[key] ??
        (resolved.additionalProperties && resolved.additionalProperties !== true
          ? resolved.additionalProperties
          : inferSchemaFromValue(value?.[key]));
      return buildSchemaNode(childPath, key, childSchema, value?.[key], depth + 1, query);
    });
  }

  function buildSchemaNode(path, key, schema, value, depth, query) {
    const resolved = resolveSchema(schema && Object.keys(schema).length > 0 ? schema : inferSchemaFromValue(value));
    const label = resolved.title ?? humanizeKey(key);
    const description = resolved.description ?? '';
    const defaultValue = hasOwn(resolved, 'default') ? cloneValue(resolved.default) : undefined;
    const inputKind = getInputKind(resolved, value);
    const dirtyFromOriginal = !valuesEqual(value, getNestedValue(originalConfig, path));
    const modifiedFromDefault = defaultValue !== undefined && !valuesEqual(value, defaultValue);
    const matchesSelf = isSearchMatch(query, key, label, description, path);

    if (inputKind === 'object') {
      const children = buildObjectChildren(path, resolved, value, depth, query);
      const visibleChildren = children.filter((child) => child.visible);
      const subtreeMatches = matchesSelf || visibleChildren.some((child) => child.subtreeMatches);
      return {
        id: path,
        path,
        key,
        label,
        description,
        defaultValue,
        dirtyFromOriginal,
        modifiedFromDefault,
        inputKind,
        depth,
        children,
        visibleChildren,
        visible: query ? subtreeMatches : true,
        matchesSelf,
        subtreeMatches,
        sensitive: false
      };
    }

    const visible = query ? matchesSelf : true;
    return {
      id: path,
      path,
      key,
      label,
      description,
      defaultValue,
      currentValue: value,
      dirtyFromOriginal,
      modifiedFromDefault,
      inputKind,
      depth,
      visible,
      matchesSelf,
      subtreeMatches: visible,
      enumOptions: getEnumOptions(resolved),
      schema: resolved,
      sensitive: isSensitiveKey(key)
    };
  }

  function buildGroupViewModels(query) {
    const normalizedQuery = query.trim().toLowerCase();
    const navGroups = buildConfigNavGroups(config);
    const schemaProperties = schemaDocument?.properties ?? {};

    return navGroups
      .map((navGroup) => {
        const groupKey = navGroup.groupKey;
        const groupSchema = schemaProperties[groupKey] ?? inferSchemaFromValue(config[groupKey]);
        const node = buildSchemaNode(groupKey, groupKey, groupSchema, config[groupKey], 0, normalizedQuery);
        const meta = SCHEMA[groupKey];
        return {
          ...navGroup,
          label: meta?.label ?? navGroup.label,
          defaultOpen: meta?.defaultOpen ?? false,
          icon: ICON_MAP[groupKey],
          node
        };
      })
      .filter((group) => group.node.visible);
  }

  const groupViewModels = $derived(buildGroupViewModels(searchQuery));
  const hasSearchQuery = $derived(searchQuery.trim().length > 0);
  const prettyConfig = $derived(JSON.stringify(config ?? {}, null, 2));
  const hasChanges = $derived(!valuesEqual(config, originalConfig));
  const visibleGroups = $derived(groupViewModels.filter((group) => group.node.visible));

  function isGroupOpen(group) {
    return hasSearchQuery ? group.node.subtreeMatches : openGroups.has(group.groupKey);
  }

  function isObjectOpen(node) {
    return hasSearchQuery ? node.subtreeMatches : openObjects.has(node.path);
  }

  function getChangedFields() {
    const diffs = [];

    function walk(current, baseline, prefix = '') {
      const currentIsObject = isPlainObject(current);
      const baselineIsObject = isPlainObject(baseline);
      if (currentIsObject && baselineIsObject) {
        const keys = Array.from(new Set([...Object.keys(current), ...Object.keys(baseline)]));
        for (const key of keys) {
          const path = prefix ? `${prefix}.${key}` : key;
          walk(current[key], baseline[key], path);
        }
        return;
      }

      if (!valuesEqual(current, baseline)) {
        diffs.push({
          path: prefix,
          label: humanizeKey(prefix.split('.').at(-1) ?? prefix),
          previous: baseline,
          current
        });
      }
    }

    walk(config, originalConfig);
    return diffs;
  }

  const changedFields = $derived(getChangedFields());

  function pathHasChanges(prefix) {
    return changedFields.some((change) => change.path === prefix || change.path.startsWith(`${prefix}.`));
  }

  function syncRawJsonDraft() {
    if (rawJsonDirty) return;
    rawJsonDraft = prettyConfig;
    rawJsonError = '';
  }

  function initializeOpenGroups() {
    const next = new Set();
    for (const group of buildConfigNavGroups(config)) {
      if (SCHEMA[group.groupKey]?.defaultOpen) {
        next.add(group.groupKey);
      }
    }
    openGroups = next;
  }

  function syncFileDrafts(files) {
    fileDrafts = Object.fromEntries(files.map((file) => [file.path, file.content]));
    fileSaveStates = EMPTY_FILE_SAVE_STATE;
  }

  async function loadConfigPage() {
    loading = true;
    errorMessage = '';
    try {
      const [schemaResponse, filesResponse] = await Promise.all([
        api.getConfigSchema(),
        api.getConfigFiles()
      ]);
      await loadConfigStore({ force: true });
      config = cloneValue(configStore.data) ?? {};
      originalConfig = cloneValue(configStore.data) ?? {};
      schemaDocument = schemaResponse ?? {};
      configFiles = Array.isArray(filesResponse) ? filesResponse : [];
      syncFileDrafts(configFiles);
      initializeOpenGroups();
      openObjects = new Set();
      rawJsonDirty = false;
      syncRawJsonDraft();
    } catch (error) {
      errorMessage = error instanceof Error ? error.message : t('config.loadFailed');
    } finally {
      loading = false;
    }
  }

  async function saveStructuredConfig() {
    if (!hasChanges || savingConfig) return;
    savingConfig = true;
    saveMessage = '';
    saveMessageTone = 'success';
    try {
      const partial = {};
      for (const change of changedFields) {
        setNestedValue(partial, change.path, change.current);
      }
      const result = await api.saveConfig(partial);
      updateConfigStore(cloneValue(config) ?? {});
      originalConfig = cloneValue(config) ?? {};
      rawJsonDirty = false;
      syncRawJsonDraft();
      if (result?.restart_required) {
        saveMessage = t('config.saveRestartRequired');
      } else {
        saveMessage = t('config.saveSuccess');
      }
      setTimeout(() => {
        saveMessage = '';
      }, 5000);
    } catch (error) {
      saveMessageTone = 'error';
      saveMessage = t('config.saveFailed', {
        message: error instanceof Error ? error.message : String(error)
      });
    } finally {
      savingConfig = false;
    }
  }

  async function saveRawJsonConfig() {
    if (savingConfig) return;
    savingConfig = true;
    rawJsonError = '';
    saveMessage = '';
    saveMessageTone = 'success';

    try {
      const parsed = JSON.parse(rawJsonDraft);
      const result = await api.saveConfig(parsed);
      config = cloneValue(parsed) ?? {};
      originalConfig = cloneValue(parsed) ?? {};
      updateConfigStore(cloneValue(parsed) ?? {});
      rawJsonDirty = false;
      syncRawJsonDraft();
      if (result?.restart_required) {
        saveMessage = t('config.saveRestartRequired');
      } else {
        saveMessage = t('config.saveSuccess');
      }
      setTimeout(() => {
        saveMessage = '';
      }, 5000);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      rawJsonError = message;
      saveMessageTone = 'error';
      saveMessage = t('config.saveFailed', { message });
    } finally {
      savingConfig = false;
    }
  }

  async function saveConfigFile(file) {
    const content = fileDrafts[file.path] ?? '';
    fileSaveStates = {
      ...fileSaveStates,
      [file.path]: { saving: true, error: '' }
    };

    try {
      const result = await api.saveConfigFile(file.filename, content);
      await loadConfigPage();
      saveMessageTone = 'success';
      saveMessage = result?.restart_required ? t('config.saveRestartRequired') : t('config.saveSuccess');
      setTimeout(() => {
        saveMessage = '';
      }, 5000);
    } catch (error) {
      fileSaveStates = {
        ...fileSaveStates,
        [file.path]: {
          saving: false,
          error: error instanceof Error ? error.message : String(error)
        }
      };
      return;
    }

    fileSaveStates = {
      ...fileSaveStates,
      [file.path]: { saving: false, error: '' }
    };
  }

  async function copyToClipboard(value) {
    if (typeof navigator === 'undefined' || !navigator.clipboard) return;
    try {
      await navigator.clipboard.writeText(value);
    } catch {}
  }

  function addStringArrayItem(path) {
    const current = getNestedValue(config, path);
    const next = Array.isArray(current) ? [...current, ''] : [''];
    updateField(path, next);
  }

  function updateStringArrayItem(path, index, value) {
    const current = getNestedValue(config, path);
    const next = Array.isArray(current) ? [...current] : [];
    next[index] = value;
    updateField(path, next);
  }

  function removeStringArrayItem(path, index) {
    const current = getNestedValue(config, path);
    if (!Array.isArray(current)) return;
    updateField(path, current.filter((_, itemIndex) => itemIndex !== index));
  }

  function focusHashTarget() {
    if (typeof window === 'undefined') return;
    const hash = window.location.hash.replace(/^#/, '');
    if (!hash.startsWith('config-section-')) return;
    const targetGroup = hash.replace(/^config-section-/, '');
    if (!groupViewModels.some((group) => group.groupKey === targetGroup)) return;
    focusGroup(targetGroup);
  }

  $effect(() => {
    loadConfigPage();
  });

  $effect(() => {
    syncRawJsonDraft();
  });

  $effect(() => {
    if (loading || advancedMode || groupViewModels.length === 0) return;
    queueMicrotask(() => {
      focusHashTarget();
    });
  });
</script>

{#snippet FieldControl(node)}
  {@const currentValue = getNestedValue(config, node.path)}
  {@const isRevealed = revealedFields.has(node.path)}

  {#if node.inputKind === 'boolean'}
    <button
      type="button"
      class="config-toggle {currentValue ? 'is-on' : ''}"
      onclick={() => updateField(node.path, !currentValue)}
      aria-label={node.label}
    >
      <span class="config-toggle__thumb"></span>
    </button>
  {:else if node.inputKind === 'enum'}
    <select
      class="config-input"
      value={currentValue ?? node.defaultValue ?? ''}
      onchange={(event) => {
        const nextValue = node.enumOptions.find((option) => String(option.value) === event.currentTarget.value)?.value;
        updateField(node.path, nextValue ?? event.currentTarget.value);
      }}
    >
      {#each node.enumOptions as option}
        <option value={String(option.value)}>{option.label}</option>
      {/each}
    </select>
  {:else if node.inputKind === 'number'}
    <input
      class="config-input"
      type="number"
      value={currentValue ?? node.defaultValue ?? ''}
      min={node.schema.minimum}
      max={node.schema.maximum}
      step={node.schema.multipleOf ?? (node.schema.type === 'integer' ? 1 : 'any')}
      oninput={(event) => {
        const raw = event.currentTarget.value;
        if (raw === '') {
          const nextConfig = cloneValue(config) ?? {};
          deleteNestedValue(nextConfig, node.path);
          config = nextConfig;
          return;
        }
        const nextValue = node.schema.type === 'integer' ? parseInt(raw, 10) : parseFloat(raw);
        if (!Number.isNaN(nextValue)) {
          updateField(node.path, nextValue);
        }
      }}
    />
  {:else if node.inputKind === 'string-array'}
    <div class="tag-editor">
      <div class="tag-list">
        {#each Array.isArray(currentValue) ? currentValue : [] as item, index}
          <label class="tag-chip">
            <input
              type="text"
              value={item}
              oninput={(event) => updateStringArrayItem(node.path, index, event.currentTarget.value)}
            />
            <button type="button" onclick={() => removeStringArrayItem(node.path, index)}>×</button>
          </label>
        {/each}
      </div>
      <button type="button" class="secondary-action" onclick={() => addStringArrayItem(node.path)}>
        Add tag
      </button>
    </div>
  {:else if node.inputKind === 'json'}
    <textarea
      class="config-editor"
      rows="6"
      value={JSON.stringify(currentValue ?? node.defaultValue ?? null, null, 2)}
      onblur={(event) => {
        try {
          updateField(node.path, JSON.parse(event.currentTarget.value));
        } catch {
          event.currentTarget.value = JSON.stringify(getNestedValue(config, node.path) ?? node.defaultValue ?? null, null, 2);
        }
      }}
    ></textarea>
  {:else}
    <div class="field-input-row">
      <input
        class="config-input"
        type={node.sensitive && !isRevealed ? 'password' : 'text'}
        value={currentValue ?? ''}
        placeholder={node.defaultValue !== undefined ? String(node.defaultValue) : ''}
        oninput={(event) => updateField(node.path, event.currentTarget.value)}
      />
      {#if node.sensitive}
        <button type="button" class="icon-action" onclick={() => toggleReveal(node.path)} aria-label="Toggle visibility">
          {#if isRevealed}
            <EyeOff size={16} />
          {:else}
            <Eye size={16} />
          {/if}
        </button>
      {/if}
    </div>
  {/if}
{/snippet}

{#snippet FieldNode(node)}
  {@const currentValue = getNestedValue(config, node.path)}
  <article class="config-field {node.modifiedFromDefault ? 'is-modified' : ''} {node.dirtyFromOriginal ? 'is-dirty' : ''}">
    <div class="config-field__meta">
      <div class="config-field__heading">
        <div>
          <div class="config-field__title-row">
            <h4>{node.label}</h4>
            {#if node.modifiedFromDefault}
              <span class="config-badge">Modified</span>
            {/if}
            {#if node.dirtyFromOriginal}
              <span class="config-badge config-badge--muted">Unsaved</span>
            {/if}
          </div>
          <p class="config-field__path">{node.path}</p>
        </div>
        {#if node.defaultValue !== undefined}
          <button type="button" class="ghost-action" onclick={() => resetFieldToDefault(node.path, node.defaultValue)}>
            <RotateCcw size={14} />
            Reset
          </button>
        {/if}
      </div>
      {#if node.description}
        <p class="config-field__description">{node.description}</p>
      {/if}
      <div class="config-field__hint-row">
        <span>Current: {formatValue(currentValue)}</span>
        {#if node.defaultValue !== undefined}
          <span>Default: {formatDefaultValue(node.defaultValue)}</span>
        {/if}
      </div>
    </div>
    <div class="config-field__control">
      {@render FieldControl(node)}
    </div>
  </article>
{/snippet}

{#snippet ObjectNode(node)}
  <section class="object-card {node.modifiedFromDefault || node.dirtyFromOriginal ? 'is-emphasized' : ''}">
    <button
      type="button"
      class="object-card__header"
      onclick={() => toggleObject(node.path)}
      aria-expanded={isObjectOpen(node)}
    >
      <div>
        <div class="object-card__title-row">
          <h4>{node.label}</h4>
          {#if node.modifiedFromDefault}
            <span class="config-badge">Modified</span>
          {/if}
          {#if node.dirtyFromOriginal}
            <span class="config-badge config-badge--muted">Unsaved</span>
          {/if}
        </div>
        <p class="object-card__path">{node.path}</p>
        {#if node.description}
          <p class="object-card__description">{node.description}</p>
        {/if}
      </div>
      <ChevronDown
        size={18}
        style={`transform: rotate(${isObjectOpen(node) ? 180 : 0}deg); transition: transform 0.18s ease;`}
      />
    </button>

    {#if isObjectOpen(node)}
      <div class="object-card__body" transition:slide={{ duration: 180 }}>
        {#if node.visibleChildren.length === 0}
          <p class="empty-state">No matching fields.</p>
        {:else}
          <div class="object-card__grid">
            {#each node.visibleChildren as child (child.id)}
              {#if child.inputKind === 'object'}
                {@render ObjectNode(child)}
              {:else}
                {@render FieldNode(child)}
              {/if}
            {/each}
          </div>
        {/if}
      </div>
    {/if}
  </section>
{/snippet}

<section class="config-page">
  <div class="config-header">
    <div>
      <h2>{t('config.title')}</h2>
      <p>Schema-driven editor with defaults, search, and config file management.</p>
    </div>

    <div class="config-header__actions">
      <label class="mode-switch">
        <input type="checkbox" bind:checked={advancedMode} />
        <span>Advanced mode</span>
      </label>
      <button type="button" class="secondary-action" onclick={() => copyToClipboard(prettyConfig)}>
        <Copy size={14} />
        Copy JSON
      </button>
      <button type="button" class="secondary-action" onclick={() => loadConfigPage()}>
        <RefreshCw size={14} />
        Reload
      </button>
    </div>
  </div>

  {#if loading}
    <p class="loading-state">Loading config...</p>
  {:else if errorMessage}
    <p class="error-banner">{errorMessage}</p>
  {:else if advancedMode}
    <div class="advanced-grid">
      <section class="advanced-card">
        <div class="advanced-card__header">
          <div>
            <div class="advanced-card__title">
              <FileJson2 size={18} />
              <h3>Merged JSON</h3>
            </div>
            <p>Direct editor for the merged runtime config payload.</p>
          </div>
          <div class="advanced-card__actions">
            <button type="button" class="secondary-action" onclick={() => { rawJsonDraft = prettyConfig; rawJsonDirty = false; rawJsonError = ''; }}>
              <RotateCcw size={14} />
              Reset
            </button>
            <button type="button" class="primary-action" onclick={saveRawJsonConfig} disabled={savingConfig}>
              <Save size={14} />
              {savingConfig ? 'Saving...' : 'Save JSON'}
            </button>
          </div>
        </div>
        <textarea
          class="config-editor config-editor--full"
          rows="24"
          bind:value={rawJsonDraft}
          oninput={() => {
            rawJsonDirty = true;
            rawJsonError = '';
          }}
        ></textarea>
        {#if rawJsonError}
          <p class="inline-error">{rawJsonError}</p>
        {/if}
      </section>

      <section class="advanced-card">
        <div class="advanced-card__header">
          <div>
            <div class="advanced-card__title">
              <Files size={18} />
              <h3>Config Files</h3>
            </div>
            <p>`config.toml` and `config.d/*.toml` are editable independently.</p>
          </div>
        </div>

        <div class="file-list">
          {#each configFiles as file (file.path)}
            {@const saveState = fileSaveStates[file.path]}
            <article class="file-card">
              <div class="file-card__header">
                <div>
                  <div class="file-card__title-row">
                    <h4>{file.path}</h4>
                    <span class="config-badge config-badge--muted">{file.source === 'main' ? 'config.toml' : 'config.d'}</span>
                  </div>
                  <p>{file.filename}</p>
                </div>
                <button
                  type="button"
                  class="primary-action"
                  onclick={() => saveConfigFile(file)}
                  disabled={saveState?.saving}
                >
                  <Save size={14} />
                  {saveState?.saving ? 'Saving...' : 'Save file'}
                </button>
              </div>
              <textarea
                class="config-editor"
                rows="12"
                value={fileDrafts[file.path] ?? ''}
                oninput={(event) => {
                  fileDrafts = {
                    ...fileDrafts,
                    [file.path]: event.currentTarget.value
                  };
                }}
              ></textarea>
              {#if saveState?.error}
                <p class="inline-error">{saveState.error}</p>
              {/if}
            </article>
          {/each}
        </div>
      </section>
    </div>
  {:else}
    <div class="config-shell">
      <div class="config-toolbar">
        <label class="search-box">
          <Search size={16} />
          <input type="search" bind:value={searchQuery} placeholder="Search by field name or description" />
        </label>

        <div class="config-pills">
          {#each visibleGroups as group (group.groupKey)}
            <button
              type="button"
              class="pill {activeNavGroup === group.groupKey ? 'is-active' : ''}"
              onclick={() => focusGroup(group.groupKey)}
            >
              <span>{group.label}</span>
              {#if pathHasChanges(group.groupKey)}
                <span class="pill__dot"></span>
              {/if}
            </button>
          {/each}
        </div>
      </div>

      {#if visibleGroups.length === 0}
        <p class="empty-state">No matching config items.</p>
      {:else}
        <div class="group-list">
          {#each visibleGroups as group (group.groupKey)}
            <section id={configSectionId(group.groupKey)} class="group-card">
              <button
                type="button"
                class="group-card__header"
                onclick={() => {
                  toggleGroup(group.groupKey);
                  activeNavGroup = group.groupKey;
                }}
                aria-expanded={isGroupOpen(group)}
              >
                <div class="group-card__title-row">
                  {#if group.icon}
                    <group.icon size={18} />
                  {:else}
                    <Database size={18} />
                  {/if}
                  <div>
                    <h3>{group.label}</h3>
                    <p>{group.groupKey}</p>
                  </div>
                </div>
                <div class="group-card__summary">
                  {#if group.node.modifiedFromDefault}
                    <span class="config-badge">Modified</span>
                  {/if}
                  {#if pathHasChanges(group.groupKey)}
                    <span class="config-badge config-badge--muted">Unsaved</span>
                  {/if}
                  <ChevronDown
                    size={18}
                    style={`transform: rotate(${isGroupOpen(group) ? 180 : 0}deg); transition: transform 0.18s ease;`}
                  />
                </div>
              </button>

              {#if isGroupOpen(group)}
                <div class="group-card__body" transition:slide={{ duration: 200 }}>
                  {#if group.node.inputKind === 'object'}
                    <div class="group-card__grid">
                      {#each group.node.visibleChildren as child (child.id)}
                        {#if child.inputKind === 'object'}
                          {@render ObjectNode(child)}
                        {:else}
                          {@render FieldNode(child)}
                        {/if}
                      {/each}
                    </div>
                  {:else}
                    {@render FieldNode(group.node)}
                  {/if}
                </div>
              {/if}
            </section>
          {/each}
        </div>
      {/if}
    </div>
  {/if}

  {#if !advancedMode && hasChanges && !loading}
    <div class="save-bar" transition:fade>
      <div class="save-bar__content">
        <div>
          <p>{changedFields.length} unsaved change(s)</p>
          <span>Save writes only changed keys back to the config API.</span>
        </div>
        <div class="save-bar__actions">
          <button type="button" class="secondary-action" onclick={discardStructuredChanges}>Discard</button>
          <button type="button" class="primary-action" onclick={saveStructuredConfig} disabled={savingConfig}>
            <Save size={14} />
            {savingConfig ? 'Saving...' : 'Save config'}
          </button>
        </div>
      </div>
      <div class="save-bar__changes">
        {#each changedFields as change (change.path)}
          <div class="change-row">
            <span>{change.path}</span>
            <code>{formatValue(change.previous)}</code>
            <span>→</span>
            <code>{formatValue(change.current)}</code>
          </div>
        {/each}
      </div>
    </div>
  {/if}

  {#if saveMessage}
    <div class="toast {saveMessageTone === 'error' ? 'is-error' : ''}" transition:fade>
      {saveMessage}
    </div>
  {/if}
</section>

<style>
  .config-page {
    display: grid;
    gap: 1.25rem;
    padding-bottom: 8rem;
  }

  .config-header,
  .config-toolbar,
  .advanced-card,
  .group-card,
  .save-bar__content,
  .save-bar__changes,
  .file-card,
  .object-card,
  .config-field {
    border: 1px solid var(--border);
    background: color-mix(in srgb, var(--bg-card) 92%, transparent);
    border-radius: 1rem;
  }

  .config-header {
    display: flex;
    justify-content: space-between;
    gap: 1rem;
    padding: 1.25rem 1.5rem;
    align-items: flex-start;
  }

  .config-header h2,
  .advanced-card__title h3,
  .group-card__title-row h3 {
    margin: 0;
  }

  .config-header p,
  .advanced-card__header p,
  .group-card__title-row p,
  .config-field__description,
  .object-card__description,
  .file-card__header p {
    margin: 0.35rem 0 0;
    color: var(--text-secondary);
    font-size: 0.92rem;
  }

  .config-header__actions,
  .advanced-card__actions,
  .save-bar__actions,
  .config-field__heading,
  .file-card__header,
  .group-card__summary {
    display: flex;
    align-items: center;
    gap: 0.75rem;
  }

  .mode-switch,
  .search-box,
  .pill,
  .primary-action,
  .secondary-action,
  .ghost-action,
  .icon-action {
    display: inline-flex;
    align-items: center;
    gap: 0.5rem;
    border-radius: 999px;
    font: inherit;
  }

  .mode-switch {
    padding: 0.65rem 0.9rem;
    border: 1px solid var(--border);
    background: var(--bg-elevated);
    cursor: pointer;
  }

  .mode-switch input {
    accent-color: var(--accent);
  }

  .primary-action,
  .secondary-action,
  .ghost-action,
  .icon-action {
    border: 1px solid var(--border);
    cursor: pointer;
    transition: background 0.2s ease, border-color 0.2s ease, color 0.2s ease;
  }

  .primary-action {
    background: var(--accent);
    border-color: color-mix(in srgb, var(--accent) 80%, black);
    color: white;
    padding: 0.7rem 1rem;
  }

  .primary-action:hover:not(:disabled) {
    background: color-mix(in srgb, var(--accent) 88%, white);
  }

  .secondary-action,
  .ghost-action,
  .icon-action {
    background: var(--bg-card);
    color: var(--text-primary);
  }

  .secondary-action {
    padding: 0.7rem 1rem;
  }

  .ghost-action {
    padding: 0.55rem 0.8rem;
    font-size: 0.88rem;
  }

  .icon-action {
    padding: 0.75rem;
  }

  .secondary-action:hover,
  .ghost-action:hover,
  .icon-action:hover {
    border-color: var(--border-hover);
    background: color-mix(in srgb, var(--bg-elevated) 85%, var(--accent) 15%);
  }

  .primary-action:disabled,
  .secondary-action:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  .config-shell {
    display: grid;
    gap: 1rem;
  }

  .config-toolbar {
    position: sticky;
    top: 0;
    z-index: 20;
    padding: 1rem;
    backdrop-filter: blur(14px);
  }

  .search-box {
    width: min(30rem, 100%);
    padding: 0.85rem 1rem;
    border: 1px solid var(--border);
    background: var(--bg-elevated);
  }

  .search-box input {
    width: 100%;
    border: 0;
    background: transparent;
    color: inherit;
    font: inherit;
  }

  .config-pills {
    display: flex;
    flex-wrap: wrap;
    gap: 0.65rem;
    margin-top: 1rem;
  }

  .pill {
    padding: 0.55rem 0.85rem;
    border: 1px solid var(--border);
    background: var(--bg-card);
    color: var(--text-secondary);
    cursor: pointer;
  }

  .pill.is-active,
  .pill:hover {
    border-color: color-mix(in srgb, var(--accent) 45%, var(--border));
    color: var(--accent);
  }

  .pill__dot {
    width: 0.5rem;
    height: 0.5rem;
    border-radius: 999px;
    background: var(--accent);
  }

  .group-list,
  .group-card__grid,
  .object-card__grid,
  .file-list {
    display: grid;
    gap: 1rem;
  }

  .group-card__header,
  .object-card__header {
    width: 100%;
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    gap: 1rem;
    padding: 1.1rem 1.2rem;
    border: 0;
    background: transparent;
    color: inherit;
    text-align: left;
    cursor: pointer;
  }

  .group-card__title-row,
  .advanced-card__title,
  .file-card__title-row,
  .object-card__title-row,
  .config-field__title-row {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    flex-wrap: wrap;
  }

  .group-card__title-row p,
  .object-card__path,
  .config-field__path {
    font-family: "IBM Plex Mono", monospace;
    font-size: 0.78rem;
    color: var(--text-muted);
  }

  .group-card__body,
  .object-card__body,
  .advanced-card,
  .file-card,
  .config-field {
    overflow: hidden;
  }

  .group-card__body,
  .object-card__body,
  .advanced-card,
  .file-card {
    padding: 0 1.2rem 1.2rem;
  }

  .config-field,
  .object-card {
    position: relative;
  }

  .config-field {
    display: grid;
    grid-template-columns: minmax(0, 1.15fr) minmax(16rem, 0.85fr);
    gap: 1rem;
    padding: 1rem;
  }

  .config-field.is-modified,
  .object-card.is-emphasized,
  .config-badge {
    display: inline-flex;
    align-items: center;
    padding: 0.2rem 0.55rem;
    border-radius: 999px;
    background: color-mix(in srgb, var(--accent) 14%, transparent);
    color: var(--accent);
    font-size: 0.75rem;
    font-weight: 600;
  }

  .config-badge--muted {
    background: var(--bg-elevated);
    color: var(--text-secondary);
  }

  .config-field__meta {
    display: grid;
    gap: 0.75rem;
  }

  .config-field__heading {
    justify-content: space-between;
  }

  .config-field__hint-row {
    display: flex;
    flex-wrap: wrap;
    gap: 0.65rem 1rem;
    color: var(--text-muted);
    font-size: 0.8rem;
  }

  .config-input,
  .config-editor,
  .tag-chip input {
    width: 100%;
    border-radius: 0.9rem;
    border: 1px solid var(--border);
    background: var(--bg-elevated);
    color: inherit;
    padding: 0.8rem 0.95rem;
    font: inherit;
  }

  .config-input:focus,
  .config-editor:focus,
  .tag-chip input:focus,
  .search-box:focus-within {
    border-color: color-mix(in srgb, var(--accent) 55%, var(--border));
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 16%, transparent);
  }

  .config-editor {
    resize: vertical;
    font-family: "IBM Plex Mono", monospace;
    line-height: 1.5;
  }

  .config-editor--full {
    min-height: 24rem;
  }

  .field-input-row {
    display: flex;
    align-items: center;
    gap: 0.6rem;
  }

  .tag-editor {
    display: grid;
    gap: 0.75rem;
  }

  .tag-list {
    display: flex;
    flex-wrap: wrap;
    gap: 0.65rem;
  }

  .tag-chip {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.3rem;
    border-radius: 999px;
    background: var(--bg-elevated);
    border: 1px solid var(--border);
  }

  .tag-chip input {
    min-width: 8rem;
    border: 0;
    padding: 0.35rem 0.55rem;
    background: transparent;
  }

  .tag-chip button {
    border: 0;
    background: transparent;
    color: var(--text-secondary);
    cursor: pointer;
    font-size: 1rem;
  }

  .config-toggle {
    position: relative;
    width: 3.25rem;
    height: 1.9rem;
    border: 0;
    border-radius: 999px;
    background: var(--border-hover);
    cursor: pointer;
    transition: background 0.2s ease;
  }

  .config-toggle.is-on {
    background: var(--accent);
  }

  .config-toggle__thumb {
    position: absolute;
    top: 0.22rem;
    left: 0.22rem;
    width: 1.45rem;
    height: 1.45rem;
    border-radius: 999px;
    background: white;
    transition: transform 0.2s ease;
  }

  .config-toggle.is-on .config-toggle__thumb {
    transform: translateX(1.35rem);
  }

  .advanced-grid {
    display: grid;
    grid-template-columns: minmax(0, 1.25fr) minmax(0, 1fr);
    gap: 1rem;
  }

  .advanced-card {
    padding: 1.2rem;
  }

  .advanced-card__header {
    display: flex;
    justify-content: space-between;
    gap: 1rem;
    margin-bottom: 1rem;
  }

  .file-card {
    padding: 1rem;
  }

  .save-bar {
    position: fixed;
    left: 1.25rem;
    right: 1.25rem;
    bottom: 1.25rem;
    z-index: 40;
    display: grid;
    gap: 0.75rem;
  }

  .save-bar__content,
  .save-bar__changes {
    padding: 1rem 1.1rem;
    backdrop-filter: blur(16px);
  }

  .save-bar__content {
    display: flex;
    justify-content: space-between;
    gap: 1rem;
  }

  .save-bar__content p,
  .save-bar__content span,
  .change-row {
    margin: 0;
    font-size: 0.92rem;
  }

  .save-bar__content span,
  .inline-error,
  .empty-state,
  .loading-state {
    color: var(--text-secondary);
  }

  .save-bar__changes {
    max-height: 14rem;
    overflow: auto;
    display: grid;
    gap: 0.55rem;
  }

  .change-row {
    display: flex;
    align-items: center;
    gap: 0.55rem;
    flex-wrap: wrap;
  }

  .change-row code {
    padding: 0.18rem 0.45rem;
    border-radius: 0.45rem;
    background: var(--bg-elevated);
    font-family: "IBM Plex Mono", monospace;
  }

  .toast,
  .error-banner {
    border-radius: 0.9rem;
    padding: 0.9rem 1rem;
    border: 1px solid color-mix(in srgb, var(--success) 30%, var(--border));
    background: color-mix(in srgb, var(--success) 12%, transparent);
    color: var(--text-primary);
  }

  .toast {
    position: fixed;
    left: 50%;
    bottom: 10rem;
    transform: translateX(-50%);
    z-index: 50;
    min-width: min(32rem, calc(100vw - 2rem));
    text-align: center;
  }

  .toast.is-error,
  .error-banner,
  .inline-error {
    border-color: color-mix(in srgb, var(--error) 30%, var(--border));
    background: color-mix(in srgb, var(--error) 10%, transparent);
    color: var(--error);
  }

  .inline-error {
    margin: 0.6rem 0 0;
    padding: 0.65rem 0.8rem;
    border-radius: 0.8rem;
    border: 1px solid color-mix(in srgb, var(--error) 30%, var(--border));
  }

  .loading-state,
  .empty-state {
    padding: 1rem;
  }

  @media (max-width: 960px) {
    .advanced-grid,
    .config-field {
      grid-template-columns: 1fr;
    }

    .config-header,
    .advanced-card__header,
    .save-bar__content,
    .file-card__header {
      flex-direction: column;
    }

    .config-toolbar {
      top: 0.5rem;
    }

    .save-bar {
      left: 0.75rem;
      right: 0.75rem;
      bottom: 0.75rem;
    }
  }
</style>
