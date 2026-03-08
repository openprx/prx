<script>
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

  // ── Schema Definition (hardcoded, Chinese descriptions) ────────
  const SCHEMA = {
    provider: {
      label: 'Provider 设置',
      defaultOpen: true,
      fields: {
        api_key: { type: 'string', sensitive: true, label: 'API Key', desc: '当前 Provider 的 API 密钥。修改后需要重启生效', default: '' },
        api_url: { type: 'string', label: 'API URL', desc: '自定义 API 端点地址。留空使用 Provider 默认值（如 Ollama 填 http://localhost:11434）', default: '' },
        default_provider: { type: 'enum', label: '默认 Provider', desc: '选择 AI 模型提供商。决定使用哪个 API 来处理请求', default: 'openrouter', options: ['openrouter', 'anthropic', 'openai', 'ollama', 'gemini', 'groq', 'glm', 'xai', 'compatible', 'copilot', 'claude-cli', 'dashscope', 'dashscope-coding-intl', 'deepseek', 'fireworks', 'mistral', 'together'] },
        default_model: { type: 'string', label: '默认模型', desc: '默认使用的模型名称（如 anthropic/claude-sonnet-4-6）', default: 'anthropic/claude-sonnet-4.6' },
        default_temperature: { type: 'number', label: '温度', desc: '模型输出的随机性（0=确定性，2=最随机）。推荐日常对话 0.7，代码任务 0.3', default: 0.7, min: 0, max: 2, step: 0.1 },
      }
    },
    gateway: {
      label: 'Gateway 网关',
      defaultOpen: true,
      fields: {
        'gateway.port': { type: 'number', label: '端口', desc: 'Gateway HTTP 服务端口号', default: 3000, min: 1, max: 65535 },
        'gateway.host': { type: 'string', label: '监听地址', desc: '绑定的 IP 地址。127.0.0.1 仅本机访问，0.0.0.0 允许外部访问', default: '127.0.0.1' },
        'gateway.require_pairing': { type: 'bool', label: '需要配对', desc: '开启后必须先配对才能访问 API。关闭则任何人可直接访问（不安全）', default: true },
        'gateway.allow_public_bind': { type: 'bool', label: '允许公网绑定', desc: '允许绑定到非 localhost 地址而不需要隧道。通常不建议开启', default: false },
        'gateway.trust_forwarded_headers': { type: 'bool', label: '信任代理头', desc: '信任 X-Forwarded-For / X-Real-IP 头。仅在反向代理后方启用', default: false },
        'gateway.request_timeout_secs': { type: 'number', label: '请求超时(秒)', desc: 'HTTP 请求处理超时时间', default: 60, min: 5, max: 600 },
        'gateway.pair_rate_limit_per_minute': { type: 'number', label: '配对速率限制(/分)', desc: '每客户端每分钟最大配对请求数', default: 10, min: 1, max: 100 },
        'gateway.webhook_rate_limit_per_minute': { type: 'number', label: 'Webhook 速率限制(/分)', desc: '每客户端每分钟最大 Webhook 请求数', default: 60, min: 1, max: 1000 },
      }
    },
    channels: {
      label: '消息通道',
      defaultOpen: true,
      fields: {
        'channels_config.message_timeout_secs': { type: 'number', label: '消息处理超时(秒)', desc: '单条消息处理的最大超时时间（LLM + 工具调用）', default: 300, min: 30, max: 3600 },
        'channels_config.cli': { type: 'bool', label: 'CLI 交互模式', desc: '启用命令行交互通道', default: true },
      }
    },
    agent: {
      label: 'Agent 编排',
      defaultOpen: false,
      fields: {
        'agent.max_tool_iterations': { type: 'number', label: '最大工具循环次数', desc: '每条用户消息最多执行多少轮工具调用。设 0 回退到默认 10', default: 10, min: 0, max: 100 },
        'agent.max_history_messages': { type: 'number', label: '最大历史消息数', desc: '每个会话保留的历史消息条数', default: 50, min: 5, max: 500 },
        'agent.parallel_tools': { type: 'bool', label: '并行工具执行', desc: '允许在单次迭代中并行调用多个工具', default: false },
        'agent.compact_context': { type: 'bool', label: '紧凑上下文', desc: '为小模型（13B 以下）减少上下文大小', default: false },
        'agent.compaction.mode': { type: 'enum', label: '上下文压缩模式', desc: 'off=不压缩，safeguard=保守压缩（默认），aggressive=激进截断', default: 'safeguard', options: ['off', 'safeguard', 'aggressive'] },
        'agent.compaction.max_context_tokens': { type: 'number', label: '最大上下文 Token', desc: '触发压缩的 Token 阈值', default: 128000, min: 1000, max: 1000000 },
        'agent.compaction.keep_recent_messages': { type: 'number', label: '压缩后保留消息数', desc: '压缩后保留最近的非系统消息数量', default: 12, min: 1, max: 100 },
        'agent.compaction.memory_flush': { type: 'bool', label: '压缩前刷新记忆', desc: '在压缩之前提取并保存记忆', default: true },
      }
    },
    memory: {
      label: '记忆存储',
      defaultOpen: false,
      fields: {
        'memory.backend': { type: 'enum', label: '存储后端', desc: '记忆存储引擎类型', default: 'sqlite', options: ['sqlite', 'postgres', 'markdown', 'lucid', 'none'] },
        'memory.auto_save': { type: 'bool', label: '自动保存', desc: '自动保存用户输入到记忆', default: true },
        'memory.hygiene_enabled': { type: 'bool', label: '记忆清理', desc: '定期运行记忆归档和保留清理', default: true },
        'memory.archive_after_days': { type: 'number', label: '归档天数', desc: '超过此天数的日志/会话文件将被归档', default: 7, min: 1, max: 365 },
        'memory.purge_after_days': { type: 'number', label: '清除天数', desc: '归档文件超过此天数后被清除', default: 30, min: 1, max: 3650 },
        'memory.conversation_retention_days': { type: 'number', label: '对话保留天数', desc: 'SQLite 后端：超过此天数的对话记录被清理', default: 3, min: 1, max: 365 },
        'memory.embedding_provider': { type: 'enum', label: '嵌入提供商', desc: '记忆向量化的嵌入模型提供商', default: 'none', options: ['none', 'openai', 'custom'] },
        'memory.embedding_model': { type: 'string', label: '嵌入模型', desc: '嵌入模型名称（如 text-embedding-3-small）', default: 'text-embedding-3-small' },
        'memory.embedding_dimensions': { type: 'number', label: '嵌入维度', desc: '嵌入向量的维度数', default: 1536, min: 64, max: 4096 },
        'memory.vector_weight': { type: 'number', label: '向量权重', desc: '混合搜索中向量相似度的权重（0-1）', default: 0.7, min: 0, max: 1, step: 0.1 },
        'memory.keyword_weight': { type: 'number', label: '关键词权重', desc: '混合搜索中 BM25 关键词匹配的权重（0-1）', default: 0.3, min: 0, max: 1, step: 0.1 },
        'memory.min_relevance_score': { type: 'number', label: '最低相关性分数', desc: '低于此分数的记忆不会注入上下文', default: 0.4, min: 0, max: 1, step: 0.05 },
        'memory.snapshot_enabled': { type: 'bool', label: '记忆快照', desc: '定期将核心记忆导出为 MEMORY_SNAPSHOT.md', default: false },
        'memory.auto_hydrate': { type: 'bool', label: '自动恢复', desc: '当 brain.db 不存在时自动从快照恢复', default: true },
      }
    },
    security: {
      label: '安全策略',
      defaultOpen: false,
      fields: {
        'autonomy.level': { type: 'enum', label: '自主级别', desc: 'read_only=只读，supervised=需审批（默认），full=完全自主', default: 'supervised', options: ['read_only', 'supervised', 'full'] },
        'autonomy.workspace_only': { type: 'bool', label: '仅工作区', desc: '限制文件写入和命令执行在工作区目录内', default: true },
        'autonomy.max_actions_per_hour': { type: 'number', label: '每小时最大操作数', desc: '每小时允许的最大操作次数', default: 20, min: 1, max: 10000 },
        'autonomy.require_approval_for_medium_risk': { type: 'bool', label: '中风险需审批', desc: '中等风险的 Shell 命令需要明确批准', default: true },
        'autonomy.block_high_risk_commands': { type: 'bool', label: '阻止高风险命令', desc: '即使在白名单中也阻止高风险命令', default: true },
        'autonomy.allowed_commands': { type: 'array', label: '允许的命令', desc: '允许执行的命令白名单', default: ['git', 'npm', 'cargo', 'ls', 'cat', 'grep', 'find', 'echo'] },
        'secrets.encrypt': { type: 'bool', label: '加密密钥', desc: '对 config.toml 中的 API Key 和 Token 进行加密存储', default: true },
      }
    },
    heartbeat: {
      label: '心跳检测',
      defaultOpen: false,
      fields: {
        'heartbeat.enabled': { type: 'bool', label: '启用心跳', desc: '启用定期心跳检查', default: false },
        'heartbeat.interval_minutes': { type: 'number', label: '间隔(分钟)', desc: '心跳检查的时间间隔', default: 30, min: 1, max: 1440 },
        'heartbeat.active_hours': { type: 'array', label: '活跃时段', desc: '心跳检查的有效小时范围（如 [8, 23]）', default: [8, 23] },
        'heartbeat.prompt': { type: 'string', label: '心跳提示词', desc: '心跳触发时使用的提示词', default: 'Check HEARTBEAT.md and follow instructions.' },
      }
    },
    reliability: {
      label: '可靠性',
      defaultOpen: false,
      fields: {
        'reliability.provider_retries': { type: 'number', label: 'Provider 重试次数', desc: '调用 Provider 失败后的重试次数', default: 2, min: 0, max: 10 },
        'reliability.provider_backoff_ms': { type: 'number', label: '重试退避(ms)', desc: 'Provider 重试的基础退避时间', default: 500, min: 100, max: 30000 },
        'reliability.fallback_providers': { type: 'array', label: '备用 Provider', desc: '主 Provider 不可用时按顺序尝试的备用列表', default: [] },
        'reliability.api_keys': { type: 'array', label: '轮换 API Key', desc: '遇到速率限制时轮换使用的额外 API Key', default: [] },
        'reliability.channel_initial_backoff_secs': { type: 'number', label: '通道初始退避(秒)', desc: '通道/守护进程重启的初始退避时间', default: 2, min: 1, max: 60 },
        'reliability.channel_max_backoff_secs': { type: 'number', label: '通道最大退避(秒)', desc: '通道/守护进程重启的最大退避时间', default: 60, min: 5, max: 3600 },
      }
    },
    scheduler: {
      label: '调度器',
      defaultOpen: false,
      fields: {
        'scheduler.enabled': { type: 'bool', label: '启用调度器', desc: '启用内置定时任务调度循环', default: true },
        'scheduler.max_tasks': { type: 'number', label: '最大任务数', desc: '最多持久化保存的计划任务数量', default: 64, min: 1, max: 1000 },
        'scheduler.max_concurrent': { type: 'number', label: '最大并发数', desc: '每次调度周期内最多执行的任务数', default: 4, min: 1, max: 32 },
        'cron.enabled': { type: 'bool', label: '启用 Cron', desc: '启用 Cron 子系统', default: true },
        'cron.max_run_history': { type: 'number', label: 'Cron 历史记录数', desc: '保留的 Cron 运行历史记录条数', default: 50, min: 10, max: 1000 },
      }
    },
    sessions_spawn: {
      label: '子进程管理',
      defaultOpen: false,
      fields: {
        'sessions_spawn.default_mode': { type: 'enum', label: '默认模式', desc: '子进程默认执行模式', default: 'task', options: ['task', 'process'] },
        'sessions_spawn.max_concurrent': { type: 'number', label: '最大并发数', desc: '全局最大并发子进程/任务数', default: 4, min: 1, max: 32 },
        'sessions_spawn.max_spawn_depth': { type: 'number', label: '最大嵌套深度', desc: '子进程可以再次 spawn 的最大深度', default: 2, min: 1, max: 10 },
        'sessions_spawn.max_children_per_agent': { type: 'number', label: '每父进程最大子数', desc: '每个父会话允许的最大并发子运行数', default: 5, min: 1, max: 20 },
        'sessions_spawn.cleanup_on_complete': { type: 'bool', label: '完成后清理', desc: '进程模式完成后删除工作区目录', default: true },
      }
    },
    observability: {
      label: '可观测性',
      defaultOpen: false,
      fields: {
        'observability.backend': { type: 'enum', label: '后端', desc: '可观测性后端类型', default: 'none', options: ['none', 'log', 'prometheus', 'otel'] },
        'observability.otel_endpoint': { type: 'string', label: 'OTLP 端点', desc: 'OpenTelemetry Collector 端点 URL（仅 otel 后端）', default: '' },
        'observability.otel_service_name': { type: 'string', label: '服务名称', desc: '上报给 OTel 的服务名称', default: 'openprx' },
      }
    },
    web_search: {
      label: '网络搜索',
      defaultOpen: false,
      fields: {
        'web_search.enabled': { type: 'bool', label: '启用搜索', desc: '启用网络搜索工具', default: false },
        'web_search.provider': { type: 'enum', label: '搜索引擎', desc: '搜索提供商。DuckDuckGo 免费无 Key，Brave 需要 API Key', default: 'duckduckgo', options: ['duckduckgo', 'brave'] },
        'web_search.brave_api_key': { type: 'string', sensitive: true, label: 'Brave API Key', desc: 'Brave Search API 密钥（选 Brave 时必填）', default: '' },
        'web_search.max_results': { type: 'number', label: '最大结果数', desc: '每次搜索返回的最大结果数（1-10）', default: 5, min: 1, max: 10 },
        'web_search.fetch_enabled': { type: 'bool', label: '启用页面抓取', desc: '允许抓取和提取网页可读内容', default: true },
        'web_search.fetch_max_chars': { type: 'number', label: '抓取最大字符', desc: '网页抓取返回的最大字符数', default: 10000, min: 100, max: 100000 },
      }
    },
    cost: {
      label: '成本控制',
      defaultOpen: false,
      fields: {
        'cost.enabled': { type: 'bool', label: '启用成本追踪', desc: '启用 API 调用成本追踪和预算控制', default: false },
        'cost.daily_limit_usd': { type: 'number', label: '日限额(USD)', desc: '每日消费上限（美元）', default: 10, min: 0.1, max: 10000, step: 0.1 },
        'cost.monthly_limit_usd': { type: 'number', label: '月限额(USD)', desc: '每月消费上限（美元）', default: 100, min: 1, max: 100000, step: 1 },
        'cost.warn_at_percent': { type: 'number', label: '预警百分比', desc: '消费达到限额的多少百分比时发出警告', default: 80, min: 10, max: 100 },
      }
    },
    runtime: {
      label: '运行时',
      defaultOpen: false,
      fields: {
        'runtime.kind': { type: 'enum', label: '运行时类型', desc: '命令执行环境：native=本机，docker=容器隔离', default: 'native', options: ['native', 'docker'] },
        'runtime.reasoning_enabled': { type: 'enum', label: '推理模式', desc: '全局推理/思考模式：null=Provider 默认，true=启用，false=禁用', default: '', options: ['', 'true', 'false'] },
      }
    },
    tunnel: {
      label: '隧道',
      defaultOpen: false,
      fields: {
        'tunnel.provider': { type: 'enum', label: '隧道类型', desc: '将 Gateway 暴露到公网的隧道服务', default: 'none', options: ['none', 'cloudflare', 'tailscale', 'ngrok', 'custom'] },
      }
    },
    identity: {
      label: '身份格式',
      defaultOpen: false,
      fields: {
        'identity.format': { type: 'enum', label: '身份格式', desc: 'OpenClaw 或 AIEOS 身份文档格式', default: 'openclaw', options: ['openclaw', 'aieos'] },
      }
    },
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

  function humanizeKey(key) {
    return String(key).replace(/_/g, ' ').replace(/\b\w/g, c => c.toUpperCase());
  }

  // ── Schema helpers ─────────────────────────────────────────────

  /** Compute which top-level config keys are handled by SCHEMA */
  function getSchemaHandledKeys() {
    const handled = new Set();
    for (const group of Object.values(SCHEMA)) {
      for (const fieldPath of Object.keys(group.fields)) {
        handled.add(fieldPath.split('.')[0]);
      }
    }
    return handled;
  }

  // Pre-compute since SCHEMA is a constant
  const SCHEMA_HANDLED_KEYS = getSchemaHandledKeys();

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

  const navGroups = $derived([
    ...schemaGroups.map(([groupKey, group]) => ({
      groupKey,
      label: group.label,
      dynamic: false,
    })),
    ...dynamicGroups.map(groupKey => ({
      groupKey,
      label: humanizeKey(groupKey),
      dynamic: true,
    })),
  ]);

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

  function sectionId(groupKey) {
    return `config-section-${groupKey}`;
  }

  function focusGroup(groupKey) {
    activeNavGroup = groupKey;

    if (typeof document === 'undefined') return;

    const target = document.getElementById(sectionId(groupKey));
    if (!target) return;

    if (target instanceof HTMLDetailsElement) {
      target.open = true;
    }

    target.scrollIntoView({ behavior: 'smooth', block: 'start' });

    if (typeof history !== 'undefined') {
      history.replaceState(null, '', `#${sectionId(groupKey)}`);
    }
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
      const [configResponse, statusResponse] = await Promise.all([
        api.getConfig(),
        api.getStatus().catch(() => null)
      ]);
      config = typeof configResponse === 'object' && configResponse ? configResponse : {};
      originalConfig = deepClone(config);
      status = statusResponse;
      errorMessage = '';
    } catch (error) {
      errorMessage = error instanceof Error ? error.message : 'Failed to load config';
    } finally {
      loading = false;
    }
  }

  async function saveConfig() {
    if (!hasChanges || saving) return;
    saving = true;
    saveMessage = '';
    try {
      const partial = {};
      for (const change of changedFields) {
        setNestedValue(partial, change.fieldPath, change.newVal);
      }
      const result = await api.saveConfig(partial);
      originalConfig = deepClone(config);
      showDiff = false;
      if (result?.restart_required) {
        saveMessage = '已保存，部分设置需要重启服务后生效';
      } else {
        saveMessage = '已保存';
      }
      setTimeout(() => { saveMessage = ''; }, 5000);
    } catch (error) {
      saveMessage = '保存失败: ' + (error instanceof Error ? error.message : String(error));
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
          id={sectionId(groupKey)}
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
                id={sectionId(groupKey)}
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
    <div class="fixed bottom-20 left-1/2 z-50 -translate-x-1/2 rounded-lg border px-4 py-2 text-sm shadow-lg {saveMessage.startsWith('保存失败') ? 'border-red-500/30 bg-red-500/10 text-red-600 dark:text-red-300' : 'border-green-500/30 bg-green-500/10 text-green-700 dark:text-green-300'}">
      {saveMessage}
    </div>
  {/if}
</section>
