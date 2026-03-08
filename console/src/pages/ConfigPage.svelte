<script>
  import { fade } from 'svelte/transition';
  import {
    BadgeCheck,
    Bot,
    Brain,
    Cable,
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
    buildConfigNavGroups,
    configSectionId,
    getConfigSectionMeta,
    GENERAL_SECTION_FIELDS,
    GENERAL_SECTION_KEY,
    humanizeKey
  } from '../lib/config-nav';
  import {
    buildSectionPayload,
    configStore,
    loadConfigBundle,
    loadConfigStore,
    readSectionValue,
    updateConfigStore,
    writeSectionValue
  } from '../lib/config-store.svelte.js';
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';

  let { activeSection = '' } = $props();

  const REDACTION_MASK = '***';
  const EMPTY_FILE_SAVE_STATE = Object.freeze({});
  const MAX_RENDER_DEPTH = 1;
  const MAX_COLLAPSED_ARRAY_ITEMS = 10;

  let fullConfig = $state({});
  let fullSchema = $state({});
  let configFiles = $state([]);
  let loading = $state(true);
  let errorMessage = $state('');
  let saveMessage = $state('');
  let saveMessageTone = $state('success');
  let savingConfig = $state(false);
  let advancedMode = $state(false);
  let searchQuery = $state('');
  let revealedFields = $state(new Set());
  let expandedArrays = $state(new Set());
  let rawJsonDraft = $state('');
  let rawJsonError = $state('');
  let rawJsonDirty = $state(false);
  let fileDrafts = $state({});
  let fileSaveStates = $state(EMPTY_FILE_SAVE_STATE);
  let sectionDrafts = $state({});
  let sectionOriginals = $state({});

  const ICON_MAP = {
    general: Zap,
    agent: Bot,
    memory: Brain,
    channels_config: MessageSquare,
    security: Shield,
    gateway: Globe,
    runtime: Settings,
    observability: BarChart3,
    reliability: RefreshCw,
    scheduler: Clock,
    heartbeat: HeartPulse,
    sessions_spawn: GitBranch,
    browser: Code2,
    mcp: Cable,
    cost: DollarSign,
    identity: BadgeCheck,
    tunnel: Cable
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

  function toggleArrayExpansion(path) {
    expandedArrays = toggleSetMember(expandedArrays, path);
  }

  function isSensitiveKey(key) {
    const lower = String(key).toLowerCase();
    return ['key', 'token', 'secret', 'password', 'auth', 'credential', 'private'].some((part) =>
      lower.includes(part)
    );
  }

  function formatDefaultValue(value) {
    if (value === undefined) return t('config.noDefault');
    if (typeof value === 'string') return value.length > 0 ? value : `(${t('common.empty').toLowerCase()})`;
    return JSON.stringify(value);
  }

  function formatValue(value) {
    if (value === undefined) return `(${t('config.field.notSet').toLowerCase()})`;
    if (value === null) return 'null';
    if (value === REDACTION_MASK) return REDACTION_MASK;
    if (typeof value === 'string') return value.length > 0 ? value : `(${t('common.empty').toLowerCase()})`;
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
    if (!pointer?.startsWith?.('#/')) return null;
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

  function resolveSchema(schema, rootSchema = fullSchema, depth = 0) {
    if (!schema || typeof schema !== 'object') return {};
    if (depth > 4) return schema;

    let current = schema;

    if (current.$ref) {
      const resolved = resolveJsonPointer(rootSchema, current.$ref);
      if (resolved) {
        current = mergeSchemas(resolveSchema(resolved, rootSchema, depth + 1), {
          ...current,
          $ref: undefined
        });
      }
    }

    if (Array.isArray(current.allOf) && current.allOf.length > 0) {
      let merged = { ...current, allOf: undefined };
      for (const part of current.allOf) {
        merged = mergeSchemas(merged, resolveSchema(part, rootSchema, depth + 1));
      }
      current = merged;
    }

    return current;
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

    if (resolved.properties || (value && isPlainObject(value))) {
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

  export function resolveSubSchema(schemaDocument, sectionKey) {
    const rootSchema = schemaDocument ?? {};
    const rootProperties = rootSchema.properties ?? {};

    if (sectionKey === GENERAL_SECTION_KEY) {
      return {
        type: 'object',
        title: getConfigSectionMeta(GENERAL_SECTION_KEY).fallbackLabel,
        properties: Object.fromEntries(
          GENERAL_SECTION_FIELDS.map((fieldKey) => [
            fieldKey,
            resolveSchema(rootProperties[fieldKey] ?? inferSchemaFromValue(fullConfig[fieldKey]), rootSchema)
          ])
        )
      };
    }

    return resolveSchema(rootProperties[sectionKey] ?? inferSchemaFromValue(fullConfig[sectionKey]), rootSchema);
  }

  function getInputKind(schema, value, depth) {
    const resolved = resolveSchema(schema);
    const enumOptions = getEnumOptions(resolved);
    if (enumOptions.length > 0) return 'enum';

    const type = getSchemaType(resolved, value);
    if (type === 'boolean') return 'boolean';
    if (type === 'number' || type === 'integer') return 'number';
    if (type === 'string') return 'string';
    if (type === 'array') {
      const itemType = getSchemaType(resolved.items, Array.isArray(value) ? value[0] : undefined);
      return itemType === 'string' ? 'string-array' : 'json';
    }
    if (type === 'object') {
      return depth < MAX_RENDER_DEPTH ? 'object-group' : 'json';
    }
    return 'json';
  }

  function getAbsolutePath(sectionKey, relativePath) {
    if (!relativePath) return sectionKey;
    if (sectionKey === GENERAL_SECTION_KEY) return relativePath;
    return `${sectionKey}.${relativePath}`;
  }

  function buildObjectChildren(sectionKey, path, schema, value, depth, query, originalSection) {
    if (depth >= MAX_RENDER_DEPTH) return [];
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
      return buildSchemaNode(sectionKey, childPath, key, childSchema, value?.[key], depth + 1, query, originalSection);
    });
  }

  function buildSchemaNode(sectionKey, path, key, schema, value, depth, query, originalSection) {
    const resolved = resolveSchema(schema && Object.keys(schema).length > 0 ? schema : inferSchemaFromValue(value));
    const label = resolved.title ?? humanizeKey(key);
    const description = resolved.description ?? '';
    const defaultValue = hasOwn(resolved, 'default') ? cloneValue(resolved.default) : undefined;
    const inputKind = getInputKind(resolved, value, depth);
    const dirtyFromOriginal = !valuesEqual(value, getNestedValue(originalSection, path));
    const modifiedFromDefault = defaultValue !== undefined && !valuesEqual(value, defaultValue);
    const absolutePath = getAbsolutePath(sectionKey, path);
    const matchesSelf = isSearchMatch(query, key, label, description, absolutePath);

    if (inputKind === 'object-group') {
      const children = buildObjectChildren(sectionKey, path, resolved, value, depth, query, originalSection);
      const visibleChildren = children.filter((child) => child.visible);
      const subtreeMatches = matchesSelf || visibleChildren.some((child) => child.subtreeMatches);
      return {
        id: absolutePath,
        path,
        absolutePath,
        key,
        label,
        description,
        defaultValue,
        dirtyFromOriginal,
        modifiedFromDefault,
        inputKind,
        children,
        visibleChildren,
        visible: query ? subtreeMatches : true,
        subtreeMatches,
        matchesSelf
      };
    }

    const visible = query ? matchesSelf : true;
    return {
      id: absolutePath,
      path,
      absolutePath,
      key,
      label,
      description,
      defaultValue,
      currentValue: value,
      dirtyFromOriginal,
      modifiedFromDefault,
      inputKind,
      visible,
      subtreeMatches: visible,
      enumOptions: getEnumOptions(resolved),
      schema: resolved,
      sensitive: isSensitiveKey(key)
    };
  }

  function getCurrentSectionDraft() {
    return sectionDrafts[resolvedSectionKey] ?? readSectionValue(fullConfig, resolvedSectionKey);
  }

  function getCurrentSectionOriginal() {
    return sectionOriginals[resolvedSectionKey] ?? readSectionValue(fullConfig, resolvedSectionKey);
  }

  function buildCurrentSectionNodes() {
    if (!resolvedSectionKey) return [];

    const draft = getCurrentSectionDraft();
    const original = getCurrentSectionOriginal();
    const query = searchQuery.trim().toLowerCase();
    const sectionSchema = resolveSubSchema(fullSchema, resolvedSectionKey);
    const propertyMap = resolveSchema(sectionSchema).properties ?? {};
    const schemaKeys = Object.keys(propertyMap);
    const valueKeys = isPlainObject(draft) ? Object.keys(draft) : [];
    const orderedKeys = [...schemaKeys, ...valueKeys.filter((key) => !schemaKeys.includes(key))];

    return orderedKeys
      .map((key) =>
        buildSchemaNode(
          resolvedSectionKey,
          key,
          key,
          propertyMap[key] ?? inferSchemaFromValue(draft?.[key]),
          draft?.[key],
          0,
          query,
          original
        )
      )
      .filter((node) => node.visible);
  }

  function collectSectionChanges(sectionKey) {
    const current = sectionDrafts[sectionKey] ?? readSectionValue(fullConfig, sectionKey);
    const baseline = sectionOriginals[sectionKey] ?? readSectionValue(fullConfig, sectionKey);
    const changes = [];

    function walk(currentValue, baselineValue, prefix = '') {
      const currentIsObject = isPlainObject(currentValue);
      const baselineIsObject = isPlainObject(baselineValue);

      if (currentIsObject && baselineIsObject) {
        const keys = Array.from(new Set([...Object.keys(currentValue), ...Object.keys(baselineValue)]));
        for (const key of keys) {
          const nextPrefix = prefix ? `${prefix}.${key}` : key;
          walk(currentValue[key], baselineValue[key], nextPrefix);
        }
        return;
      }

      if (!valuesEqual(currentValue, baselineValue)) {
        changes.push({
          path: prefix,
          absolutePath: getAbsolutePath(sectionKey, prefix),
          previous: baselineValue,
          current: currentValue
        });
      }
    }

    walk(current, baseline);
    return changes;
  }

  const navGroups = $derived(buildConfigNavGroups(fullConfig));
  const resolvedSectionKey = $derived(
    navGroups.some((group) => group.groupKey === activeSection) ? activeSection : (navGroups[0]?.groupKey ?? '')
  );
  const currentSectionMeta = $derived(getConfigSectionMeta(resolvedSectionKey || GENERAL_SECTION_KEY));
  const currentSectionLabel = $derived(
    currentSectionMeta.labelKey ? t(currentSectionMeta.labelKey) : currentSectionMeta.fallbackLabel
  );
  const currentSectionIcon = $derived(ICON_MAP[resolvedSectionKey] ?? Database);
  const currentSectionSchema = $derived(resolveSubSchema(fullSchema, resolvedSectionKey));
  const currentSectionDraft = $derived(getCurrentSectionDraft());
  const currentSectionNodes = $derived(buildCurrentSectionNodes());
  const changedFields = $derived(collectSectionChanges(resolvedSectionKey));
  const hasChanges = $derived(changedFields.length > 0);
  const prettyConfig = $derived(JSON.stringify(fullConfig ?? {}, null, 2));

  function ensureSectionState(sectionKey) {
    if (!sectionKey || loading) return;
    if (hasOwn(sectionDrafts, sectionKey) && hasOwn(sectionOriginals, sectionKey)) return;

    const sectionValue = readSectionValue(fullConfig, sectionKey);
    sectionDrafts = {
      ...sectionDrafts,
      [sectionKey]: cloneValue(sectionValue) ?? {}
    };
    sectionOriginals = {
      ...sectionOriginals,
      [sectionKey]: cloneValue(sectionValue) ?? {}
    };
  }

  function setCurrentSectionDraft(nextSectionValue) {
    sectionDrafts = {
      ...sectionDrafts,
      [resolvedSectionKey]: cloneValue(nextSectionValue) ?? {}
    };
    fullConfig = writeSectionValue(fullConfig, resolvedSectionKey, nextSectionValue);
  }

  function updateField(path, value) {
    const nextSection = cloneValue(currentSectionDraft) ?? {};
    setNestedValue(nextSection, path, value);
    setCurrentSectionDraft(nextSection);
  }

  function resetFieldToDefault(path, defaultValue) {
    if (defaultValue === undefined) return;
    updateField(path, cloneValue(defaultValue));
  }

  function discardStructuredChanges() {
    const originalSection = cloneValue(getCurrentSectionOriginal()) ?? {};
    sectionDrafts = {
      ...sectionDrafts,
      [resolvedSectionKey]: originalSection
    };
    fullConfig = writeSectionValue(fullConfig, resolvedSectionKey, originalSection);
    rawJsonDirty = false;
    rawJsonError = '';
  }

  function syncRawJsonDraft() {
    if (rawJsonDirty) return;
    rawJsonDraft = prettyConfig;
    rawJsonError = '';
  }

  function syncFileDrafts(files) {
    fileDrafts = Object.fromEntries(files.map((file) => [file.path, file.content]));
    fileSaveStates = EMPTY_FILE_SAVE_STATE;
  }

  async function loadConfigPage({ force = true } = {}) {
    loading = true;
    errorMessage = '';
    try {
      const [bundle, filesResponse] = await Promise.all([loadConfigBundle({ force }), api.getConfigFiles()]);
      fullConfig = cloneValue(bundle.config) ?? {};
      fullSchema = bundle.schema ?? {};
      configFiles = Array.isArray(filesResponse) ? filesResponse : [];
      sectionDrafts = {};
      sectionOriginals = {};
      syncFileDrafts(configFiles);
      rawJsonDirty = false;
      syncRawJsonDraft();
    } catch (error) {
      errorMessage = error instanceof Error ? error.message : t('config.loadFailed');
    } finally {
      loading = false;
    }
  }

  async function saveStructuredConfig() {
    if (!resolvedSectionKey || !hasChanges || savingConfig) return;
    savingConfig = true;
    saveMessage = '';
    saveMessageTone = 'success';

    try {
      const payload = buildSectionPayload(resolvedSectionKey, currentSectionDraft);
      const result = await api.saveConfig(payload);
      const refreshedConfig = cloneValue(await loadConfigStore({ force: true })) ?? {};
      updateConfigStore(refreshedConfig);
      fullConfig = refreshedConfig;

      const refreshedSection = cloneValue(readSectionValue(refreshedConfig, resolvedSectionKey)) ?? {};
      sectionDrafts = {
        ...sectionDrafts,
        [resolvedSectionKey]: refreshedSection
      };
      sectionOriginals = {
        ...sectionOriginals,
        [resolvedSectionKey]: cloneValue(refreshedSection) ?? {}
      };

      rawJsonDirty = false;
      syncRawJsonDraft();
      saveMessage = result?.restart_required ? t('config.saveRestartRequired') : t('config.saveSuccess');
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
      const nextConfig = cloneValue(parsed) ?? {};
      fullConfig = nextConfig;
      updateConfigStore(nextConfig);
      sectionDrafts = {};
      sectionOriginals = {};
      rawJsonDirty = false;
      syncRawJsonDraft();
      saveMessage = result?.restart_required ? t('config.saveRestartRequired') : t('config.saveSuccess');
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
      await loadConfigPage({ force: true });
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
    const current = getNestedValue(currentSectionDraft, path);
    const next = Array.isArray(current) ? [...current, ''] : [''];
    updateField(path, next);
  }

  function updateStringArrayItem(path, index, value) {
    const current = getNestedValue(currentSectionDraft, path);
    const next = Array.isArray(current) ? [...current] : [];
    next[index] = value;
    updateField(path, next);
  }

  function removeStringArrayItem(path, index) {
    const current = getNestedValue(currentSectionDraft, path);
    if (!Array.isArray(current)) return;
    updateField(
      path,
      current.filter((_, itemIndex) => itemIndex !== index)
    );
  }

  $effect(() => {
    loadConfigPage();
  });

  $effect(() => {
    syncRawJsonDraft();
  });

  $effect(() => {
    ensureSectionState(resolvedSectionKey);
  });
</script>

{#snippet FieldControl(node)}
  {@const currentValue = getNestedValue(currentSectionDraft, node.path)}
  {@const isRevealed = revealedFields.has(node.absolutePath)}

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
          const nextSection = cloneValue(currentSectionDraft) ?? {};
          deleteNestedValue(nextSection, node.path);
          setCurrentSectionDraft(nextSection);
          return;
        }
        const nextValue = node.schema.type === 'integer' ? parseInt(raw, 10) : parseFloat(raw);
        if (!Number.isNaN(nextValue)) {
          updateField(node.path, nextValue);
        }
      }}
    />
  {:else if node.inputKind === 'string-array'}
    {@const items = Array.isArray(currentValue) ? currentValue : []}
    {@const expanded = expandedArrays.has(node.absolutePath)}
    {@const visibleItems = expanded ? items : items.slice(0, MAX_COLLAPSED_ARRAY_ITEMS)}
    <div class="tag-editor">
      <div class="tag-list">
        {#each visibleItems as item, index}
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

      {#if items.length > MAX_COLLAPSED_ARRAY_ITEMS}
        <button type="button" class="ghost-action" onclick={() => toggleArrayExpansion(node.absolutePath)}>
          {expanded ? t('common.reset') : t('config.showAll', { count: items.length })}
        </button>
      {/if}

      <button type="button" class="secondary-action" onclick={() => addStringArrayItem(node.path)}>
        {t('config.addListItem')}
      </button>
    </div>
  {:else if node.inputKind === 'json'}
    <textarea
      class="config-editor"
      rows="8"
      value={JSON.stringify(currentValue ?? node.defaultValue ?? null, null, 2)}
      onblur={(event) => {
        try {
          updateField(node.path, JSON.parse(event.currentTarget.value));
        } catch {
          event.currentTarget.value = JSON.stringify(
            getNestedValue(currentSectionDraft, node.path) ?? node.defaultValue ?? null,
            null,
            2
          );
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
        <button
          type="button"
          class="icon-action"
          onclick={() => toggleReveal(node.absolutePath)}
          aria-label={t('config.toggleVisibility')}
        >
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
  {@const currentValue = getNestedValue(currentSectionDraft, node.path)}
  <article class="config-field {node.modifiedFromDefault ? 'is-modified' : ''} {node.dirtyFromOriginal ? 'is-dirty' : ''}">
    <div class="config-field__meta">
      <div class="config-field__heading">
        <div>
          <div class="config-field__title-row">
            <h4>{node.label}</h4>
            {#if node.modifiedFromDefault}
              <span class="config-badge">{t('config.modified')}</span>
            {/if}
            {#if node.dirtyFromOriginal}
              <span class="config-badge config-badge--muted">{t('config.unsaved')}</span>
            {/if}
          </div>
          <p class="config-field__path">{node.absolutePath}</p>
        </div>
        {#if node.defaultValue !== undefined}
          <button type="button" class="ghost-action" onclick={() => resetFieldToDefault(node.path, node.defaultValue)}>
            <RotateCcw size={14} />
            {t('common.reset')}
          </button>
        {/if}
      </div>
      {#if node.description}
        <p class="config-field__description">{node.description}</p>
      {/if}
      <div class="config-field__hint-row">
        <span>{t('config.currentValue')}: {formatValue(currentValue)}</span>
        {#if node.defaultValue !== undefined}
          <span>{t('config.defaultValue')}: {formatDefaultValue(node.defaultValue)}</span>
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
    <div class="object-card__header">
      <div>
        <div class="object-card__title-row">
          <h4>{node.label}</h4>
          {#if node.modifiedFromDefault}
            <span class="config-badge">{t('config.modified')}</span>
          {/if}
          {#if node.dirtyFromOriginal}
            <span class="config-badge config-badge--muted">{t('config.unsaved')}</span>
          {/if}
        </div>
        <p class="object-card__path">{node.absolutePath}</p>
        {#if node.description}
          <p class="object-card__description">{node.description}</p>
        {/if}
      </div>
    </div>

    <div class="object-card__body">
      {#if node.visibleChildren.length === 0}
        <p class="empty-state">{t('config.noMatchingFields')}</p>
      {:else}
        <div class="object-card__grid">
          {#each node.visibleChildren as child (child.id)}
            {#if child.inputKind === 'object-group'}
              {@render ObjectNode(child)}
            {:else}
              {@render FieldNode(child)}
            {/if}
          {/each}
        </div>
      {/if}
    </div>
  </section>
{/snippet}

<section class="config-page">
  <div class="config-header">
    <div>
      <h2>{t('config.title')}</h2>
      <p>{t('config.description')}</p>
    </div>

    <div class="config-header__actions">
      <label class="mode-switch">
        <input type="checkbox" bind:checked={advancedMode} />
        <span>{t('config.advancedMode')}</span>
      </label>
      <button type="button" class="secondary-action" onclick={() => copyToClipboard(prettyConfig)}>
        <Copy size={14} />
        {t('config.copyJson')}
      </button>
      <button type="button" class="secondary-action" onclick={() => loadConfigPage({ force: true })}>
        <RefreshCw size={14} />
        {t('common.reload')}
      </button>
    </div>
  </div>

  {#if loading}
    <p class="loading-state">{t('config.loading')}</p>
  {:else if errorMessage}
    <p class="error-banner">{errorMessage}</p>
  {:else if advancedMode}
    <div class="advanced-grid">
      <section class="advanced-card">
        <div class="advanced-card__header">
          <div>
            <div class="advanced-card__title">
              <FileJson2 size={18} />
              <h3>{t('config.mergedJsonTitle')}</h3>
            </div>
            <p>{t('config.mergedJsonDescription')}</p>
          </div>
          <div class="advanced-card__actions">
            <button
              type="button"
              class="secondary-action"
              onclick={() => {
                rawJsonDraft = prettyConfig;
                rawJsonDirty = false;
                rawJsonError = '';
              }}
            >
              <RotateCcw size={14} />
              {t('common.reset')}
            </button>
            <button type="button" class="primary-action" onclick={saveRawJsonConfig} disabled={savingConfig}>
              <Save size={14} />
              {savingConfig ? t('common.saving') : t('config.saveJson')}
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
              <h3>{t('config.configFilesTitle')}</h3>
            </div>
            <p>{t('config.configFilesDescription')}</p>
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
                    <span class="config-badge config-badge--muted">
                      {file.source === 'main' ? t('config.sourceMain') : t('config.sourceDirectory')}
                    </span>
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
                  {saveState?.saving ? t('common.saving') : t('config.saveFile')}
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
      <section id={configSectionId(resolvedSectionKey)} class="section-card">
        <div class="section-card__header">
          <div class="section-card__title">
            <currentSectionIcon size={18}></currentSectionIcon>
            <div>
              <h3>{currentSectionLabel}</h3>
              <p>{resolvedSectionKey}</p>
            </div>
          </div>
          <div class="section-card__summary">
            <span>{currentSectionNodes.length} field(s)</span>
            {#if hasChanges}
              <span class="config-badge config-badge--muted">{t('config.unsaved')}</span>
            {/if}
          </div>
        </div>

        <div class="config-toolbar">
          <label class="search-box">
            <Search size={16} />
            <input type="search" bind:value={searchQuery} placeholder={t('config.searchPlaceholder')} />
          </label>
          <p class="section-card__hint">
            {t('config.sectionHint', { section: currentSectionLabel })}
          </p>
        </div>

        {#if currentSectionNodes.length === 0}
          <p class="empty-state">{t('config.noMatchingItems')}</p>
        {:else}
          <div class="section-card__grid">
            {#each currentSectionNodes as node (node.id)}
              {#if node.inputKind === 'object-group'}
                {@render ObjectNode(node)}
              {:else}
                {@render FieldNode(node)}
              {/if}
            {/each}
          </div>
        {/if}
      </section>
    </div>
  {/if}

  {#if !advancedMode && hasChanges && !loading}
    <div class="save-bar" transition:fade>
      <div class="save-bar__content">
        <div>
          <p>{t('config.unsavedChangesCount', { count: changedFields.length })}</p>
          <span>{t('config.saveHint')}</span>
        </div>
        <div class="save-bar__actions">
          <button type="button" class="secondary-action" onclick={discardStructuredChanges}>
            {t('config.discard')}
          </button>
          <button type="button" class="primary-action" onclick={saveStructuredConfig} disabled={savingConfig}>
            <Save size={14} />
            {savingConfig ? t('common.saving') : t('config.saveConfig')}
          </button>
        </div>
      </div>
      <div class="save-bar__changes">
        {#each changedFields as change (change.absolutePath)}
          <div class="change-row">
            <span>{change.absolutePath}</span>
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
  .section-card,
  .save-bar__content,
  .save-bar__changes,
  .file-card,
  .object-card,
  .config-field {
    border: 1px solid var(--border);
    background: color-mix(in srgb, var(--bg-card) 92%, transparent);
    border-radius: 1rem;
  }

  .config-header,
  .section-card__header,
  .advanced-card__header,
  .file-card__header,
  .config-field__heading {
    display: flex;
    justify-content: space-between;
    gap: 1rem;
    align-items: flex-start;
  }

  .config-header,
  .advanced-card,
  .section-card,
  .file-card,
  .object-card,
  .config-field {
    padding: 1.25rem 1.5rem;
  }

  .config-header h2,
  .advanced-card__title h3,
  .section-card__title h3 {
    margin: 0;
  }

  .config-header p,
  .advanced-card__header p,
  .section-card__title p,
  .section-card__hint,
  .config-field__description,
  .object-card__description,
  .file-card__header p,
  .config-field__path,
  .object-card__path {
    margin: 0.35rem 0 0;
    color: var(--text-secondary);
    font-size: 0.92rem;
  }

  .config-header__actions,
  .advanced-card__actions,
  .save-bar__actions,
  .section-card__summary,
  .config-field__title-row,
  .object-card__title-row,
  .file-card__title-row,
  .config-field__hint-row {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    flex-wrap: wrap;
  }

  .mode-switch,
  .search-box,
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

  .config-shell,
  .advanced-grid,
  .file-list,
  .object-card__grid,
  .section-card__grid {
    display: grid;
    gap: 1rem;
  }

  .advanced-grid {
    grid-template-columns: repeat(auto-fit, minmax(320px, 1fr));
  }

  .section-card__title,
  .advanced-card__title {
    display: flex;
    align-items: center;
    gap: 0.75rem;
  }

  .config-toolbar {
    padding: 1rem;
    display: flex;
    justify-content: space-between;
    gap: 1rem;
    align-items: center;
    margin-top: 1rem;
  }

  .search-box {
    flex: 1;
    min-width: 0;
    padding: 0.8rem 1rem;
    border: 1px solid var(--border);
    background: var(--bg-elevated);
  }

  .search-box input,
  .config-input,
  .tag-chip input,
  .config-editor {
    width: 100%;
    border: none;
    background: transparent;
    color: inherit;
    font: inherit;
    outline: none;
  }

  .section-card__hint {
    min-width: 14rem;
    text-align: right;
  }

  .config-field,
  .object-card {
    display: grid;
    gap: 1rem;
  }

  .config-field__control,
  .object-card__body {
    display: grid;
    gap: 0.85rem;
  }

  .config-field__title-row h4,
  .object-card__title-row h4,
  .file-card__title-row h4 {
    margin: 0;
  }

  .config-field__hint-row {
    color: var(--text-secondary);
    font-size: 0.85rem;
  }

  .config-input,
  .config-editor,
  .tag-chip {
    border: 1px solid var(--border);
    background: var(--bg-elevated);
    border-radius: 0.9rem;
  }

  .config-input,
  .config-editor {
    padding: 0.85rem 1rem;
  }

  .config-editor {
    min-height: 10rem;
    resize: vertical;
    font-family: 'IBM Plex Mono', monospace;
    line-height: 1.5;
  }

  .config-editor--full {
    min-height: 28rem;
  }

  .field-input-row {
    display: flex;
    gap: 0.75rem;
    align-items: center;
  }

  .tag-editor {
    display: grid;
    gap: 0.75rem;
  }

  .tag-list {
    display: grid;
    gap: 0.75rem;
  }

  .tag-chip {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    padding: 0.3rem 0.85rem;
  }

  .tag-chip button {
    border: none;
    background: transparent;
    color: var(--text-secondary);
    cursor: pointer;
    font-size: 1.2rem;
    line-height: 1;
  }

  .config-toggle {
    position: relative;
    width: 3.25rem;
    height: 1.9rem;
    border: none;
    border-radius: 999px;
    background: color-mix(in srgb, var(--text-secondary) 35%, transparent);
    cursor: pointer;
    transition: background 0.2s ease;
  }

  .config-toggle.is-on {
    background: var(--accent);
  }

  .config-toggle__thumb {
    position: absolute;
    top: 0.2rem;
    left: 0.2rem;
    width: 1.5rem;
    height: 1.5rem;
    border-radius: 999px;
    background: white;
    transition: transform 0.2s ease;
  }

  .config-toggle.is-on .config-toggle__thumb {
    transform: translateX(1.35rem);
  }

  .config-badge {
    display: inline-flex;
    align-items: center;
    padding: 0.28rem 0.55rem;
    border-radius: 999px;
    background: color-mix(in srgb, var(--accent) 18%, transparent);
    color: var(--accent);
    font-size: 0.76rem;
    font-weight: 600;
  }

  .config-badge--muted {
    background: color-mix(in srgb, var(--text-secondary) 16%, transparent);
    color: var(--text-secondary);
  }

  .save-bar {
    position: fixed;
    left: 18rem;
    right: 1.5rem;
    bottom: 1.5rem;
    display: grid;
    gap: 0.75rem;
    z-index: 30;
  }

  .save-bar__content,
  .save-bar__changes {
    padding: 1rem 1.25rem;
  }

  .save-bar__content {
    display: flex;
    justify-content: space-between;
    gap: 1rem;
    align-items: center;
  }

  .save-bar__content p,
  .save-bar__content span {
    margin: 0;
  }

  .save-bar__changes {
    display: grid;
    gap: 0.55rem;
    max-height: 12rem;
    overflow: auto;
  }

  .change-row {
    display: grid;
    grid-template-columns: minmax(0, 1.3fr) minmax(0, 1fr) auto minmax(0, 1fr);
    gap: 0.75rem;
    align-items: center;
    font-size: 0.85rem;
  }

  .change-row code {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    padding: 0.25rem 0.45rem;
    border-radius: 0.5rem;
    background: var(--bg-elevated);
  }

  .loading-state,
  .empty-state,
  .error-banner,
  .inline-error {
    margin: 0;
    padding: 1rem 1.25rem;
    border-radius: 1rem;
  }

  .loading-state,
  .empty-state {
    border: 1px dashed var(--border);
    color: var(--text-secondary);
    background: color-mix(in srgb, var(--bg-card) 80%, transparent);
  }

  .error-banner,
  .inline-error,
  .toast.is-error {
    background: color-mix(in srgb, #ef4444 16%, transparent);
    color: #fca5a5;
  }

  .toast {
    position: fixed;
    right: 1.5rem;
    bottom: 1.5rem;
    padding: 0.9rem 1.2rem;
    border-radius: 0.9rem;
    background: color-mix(in srgb, #10b981 16%, var(--bg-card));
    color: #6ee7b7;
    z-index: 35;
  }

  @media (max-width: 1024px) {
    .save-bar {
      left: 1rem;
      right: 1rem;
      bottom: 1rem;
    }
  }

  @media (max-width: 720px) {
    .config-header,
    .section-card__header,
    .advanced-card__header,
    .file-card__header,
    .config-toolbar,
    .save-bar__content,
    .change-row,
    .field-input-row {
      grid-template-columns: 1fr;
      display: grid;
    }

    .section-card__hint {
      min-width: 0;
      text-align: left;
    }

    .save-bar {
      position: static;
    }
  }
</style>
