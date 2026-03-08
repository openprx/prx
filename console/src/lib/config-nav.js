export const SCHEMA = {
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

export function humanizeKey(key) {
  return String(key).replace(/_/g, ' ').replace(/\b\w/g, c => c.toUpperCase());
}

export function getSchemaHandledKeys() {
  const handled = new Set();
  for (const group of Object.values(SCHEMA)) {
    for (const fieldPath of Object.keys(group.fields)) {
      handled.add(fieldPath.split('.')[0]);
    }
  }
  return handled;
}

export const SCHEMA_HANDLED_KEYS = getSchemaHandledKeys();

export function buildConfigNavGroups(config) {
  const schemaGroups = Object.entries(SCHEMA).map(([groupKey, group]) => ({
    groupKey,
    label: group.label,
    dynamic: false,
  }));

  if (!config || typeof config !== 'object') {
    return schemaGroups;
  }

  const dynamicGroups = Object.keys(config)
    .filter(key => !SCHEMA_HANDLED_KEYS.has(key))
    .sort()
    .map(groupKey => ({
      groupKey,
      label: humanizeKey(groupKey),
      dynamic: true,
    }));

  return [...schemaGroups, ...dynamicGroups];
}

export function configSectionId(groupKey) {
  return `config-section-${groupKey}`;
}

export function focusConfigSection(groupKey) {
  if (typeof document === 'undefined' || typeof window === 'undefined') return;

  const target = document.getElementById(configSectionId(groupKey));
  if (target instanceof HTMLDetailsElement) {
    target.open = true;
  }

  if (target) {
    target.scrollIntoView({ behavior: 'smooth', block: 'start' });
  }

  const nextHash = `#${configSectionId(groupKey)}`;
  if (window.location.hash !== nextHash) {
    window.location.hash = nextHash;
  }
}
