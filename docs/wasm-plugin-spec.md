# PRX WASM Plugin System — Technical Specification

> **Version:** 0.1-draft  
> **Date:** 2026-03-07  
> **Status:** Implemented (P1-P5)  
> **Author:** David (AI Architect)  
> **Stack:** wasmtime + Component Model + WIT (不使用 Extism)

---

## 目录

1. [背景与动机](#1-背景与动机)
2. [架构概览](#2-架构概览)
3. [通信标准](#3-通信标准)
4. [WIT 接口定义](#4-wit-接口定义)
5. [Capability 详细设计](#5-capability-详细设计)
6. [Host Functions 完整清单](#6-host-functions-完整清单)
7. [多语言插件开发方案](#7-多语言插件开发方案)
8. [权限模型](#8-权限模型)
9. [性能分析](#9-性能分析)
10. [热加载设计](#10-热加载设计)
11. [插件间通信](#11-插件间通信)
12. [PDK/SDK 规划](#12-pdksdk-规划)
13. [分期实现计划](#13-分期实现计划)
14. [风险清单](#14-风险清单)

---

## 1. 背景与动机

### 1.1 PRX 现有架构

PRX 是一个 Rust + tokio 全异步 AI Agent 运行时，核心抽象：

| Trait | 位置 | 职责 |
|-------|------|------|
| `Tool` | `src/tools/traits.rs` | 工具能力：name/description/parameters_schema/execute（async） |
| `Channel` | `src/channels/traits.rs` | 消息通道：send/listen/typing/draft（async） |
| `Provider` | `src/providers/traits.rs` | LLM 推理后端：chat/stream（async） |
| `Memory` | `src/memory/traits.rs` | 持久化记忆：store/recall/forget（async） |
| `HookManager` | `src/hooks/mod.rs` | 生命周期钩子：agent_start/llm_request/tool_call 等 |

**AppState**（`src/gateway/mod.rs:296`）持有所有 trait object 的 `Arc<dyn T>` 引用，是运行时的中央状态。

**Agent Loop**（`src/agent/loop_.rs`，~4000 行）是消息处理主循环，驱动 tool call loop、streaming、approval 等。

### 1.2 定位：进程内扩展协议（Process-level ABI）

PRX WASM Plugin 是 **进程级扩展协议**，不是网络协议。插件运行在 PRX 进程内部，通过函数调用和共享线性内存与 host 通信，不经过网络栈。

**与 MCP 的关系：不同层级，互补而非替代**

| 维度 | WASM Plugin (进程级 ABI) | MCP (网络级协议) |
|------|--------------------------|-------------------|
| 通信方式 | 函数调用 + 共享线性内存 | JSON-RPC over stdio/HTTP/SSE |
| 序列化开销 | 零拷贝/零序列化（线性内存直传） | 完整 JSON 序列化/反序列化 |
| 调用延迟 | 微秒级（~1-10μs） | 毫秒级（~1-100ms） |
| 部署位置 | 同进程（in-process） | 跨进程/跨机器 |
| 适用场景 | 高频调用、性能敏感、紧密集成 | 跨语言服务、远程工具、松耦合集成 |
| 安全边界 | WASM 沙箱 + 权限门控 | 进程隔离 + 网络认证 |
| 类比 | 浏览器扩展 API、Nginx 模块 | REST API、gRPC 服务 |

**架构决策：** PRX 同时支持 WASM 插件（进程级高性能扩展）和 MCP（网络级标准互操作）。两者在不同场景下各有优势，不存在替代关系。

### 1.3 为什么选 wasmtime 直接集成

| 对比项 | Extism | wasmtime 直接 |
|--------|--------|--------------|
| async host function | ❌ 不支持 | ✅ 原生支持 (`Config::async_support`) |
| Component Model | ❌ 仅 Core Module | ✅ 原生支持 |
| WIT 类型系统 | ❌ 无 | ✅ 丰富类型（record/variant/enum/list/option/result） |
| 多语言 | ✅ 多语言 PDK | ✅ 通过 Component Model 标准工具链 |
| 控制粒度 | 低（封装层） | 高（完全控制 Store/Engine/Linker） |
| 维护方 | Dylibso | Bytecode Alliance (Mozilla/Fastly/Intel) |

**决策：** wasmtime 原生集成 + Component Model + WIT。零抽象开销，完全掌控。

---

## 2. 架构概览

### 2.1 分层架构

```
┌─────────────────────────────────────────────────────────────┐
│                    PRX Agent Runtime                        │
│      (AppState / Agent Loop / Config / Cron / etc.)         │
│                                                             │
│  ┌────────────────────┐     ┌────────────────────────────┐  │
│  │   MCP Client       │     │   Plugin Host Layer        │  │
│  │   (网络级协议)     │     │   (进程级 ABI)             │  │
│  │   JSON-RPC/HTTP    │     │   函数调用 + 共享内存       │  │
│  │   ~ms 延迟         │     │   ~μs 延迟                 │  │
│  └────────┬───────────┘     ├────────────────────────────┤  │
│           │                 │  ┌────────┬────────┬─────┐ │  │
│           │                 │  │Registry│ Loader │Event│ │  │
│           │                 │  │        │ (hot)  │ Bus │ │  │
│           │                 │  └────────┴────────┴─────┘ │  │
│           │                 ├────────────────────────────┤  │
│           │                 │  wasmtime Component Runtime │  │
│           │                 │  Engine │ Linker │ Store    │  │
│           │                 ├────────────────────────────┤  │
│           │                 │  WIT Interface Layer (ABI)  │  │
│           │                 │  prx:plugin/* (guest 导出)  │  │
│           │                 │  prx:host/*   (host 提供)   │  │
│           │                 ├────────────────────────────┤  │
│  ┌────────┴───────────┐     │  WASM Components (.wasm)   │  │
│  │ Remote MCP Servers │     │  Rust│Python│JS│Go│...     │  │
│  └────────────────────┘     └────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

**两种扩展路径对比：**
- **左侧（MCP）：** 网络级标准协议，适合远程服务、跨语言松耦合集成
- **右侧（WASM Plugin）：** 进程级 ABI，适合高频调用、性能敏感、紧密集成的扩展

### 2.2 核心组件

#### Engine（全局唯一）
- 编译配置：`Config::async_support(true)`, `Config::wasm_component_model(true)`
- 共享编译缓存，amortize 编译成本
- 支持 `Engine::precompile_component()` 预编译加速

#### Store（per-instance）
- 每个插件实例一个 Store，持有 host state
- host state 包含：权限上下文、资源配额、通道引用
- fuel-based 执行限制：`Store::set_fuel()` / `Store::fuel_consumed()`
- epoch-based 中断：`Store::epoch_deadline_async_yield_and_update()`

#### Linker（per-capability-type）
- 按 capability 类型预配置 Linker
- `bindgen!` 生成的 `add_to_linker` 注册 host functions
- 每种 capability world 一个 Linker template，实例化时 clone

#### Plugin Instance
```rust
pub struct PluginInstance {
    id: PluginId,
    manifest: PluginManifest,
    // wasmtime 实例 - 存活在 tokio task 中
    store: wasmtime::Store<HostState>,
    instance: wasmtime::component::Instance,
    // 运行时状态
    status: PluginStatus,
    stats: PluginStats,
}
```

### 2.3 HostState（传递给 Store 的状态）

```rust
pub struct HostState {
    // 插件身份
    plugin_id: PluginId,
    plugin_name: String,
    
    // 权限（申请-授权模型）
    granted_permissions: HashSet<String>,     // 已授予的权限
    optional_permissions: HashSet<String>,    // 可动态申请的权限
    
    // 资源限额
    limits: ResourceLimits,
    
    // 对 PRX 运行时的引用（通过 Arc）
    runtime: Arc<PluginRuntime>,
    
    // WASI 资源
    wasi_ctx: wasmtime_wasi::WasiCtx,
    wasi_table: wasmtime::component::ResourceTable,
    
    // 插件专属 KV 存储
    kv_namespace: String,
}
```

---

## 3. 通信标准

### 3.1 进程级 ABI 规范

WASM 插件与 PRX host 之间的通信完全在进程内完成，遵循以下 ABI 约定：

```
┌──────────────────────────────────────────────────────────┐
│                     PRX Host Process                     │
│                                                          │
│  Host (Rust)                    Guest (WASM)             │
│  ┌──────────┐                  ┌──────────────┐          │
│  │ Linker   │ ──func call──►  │ Guest Export  │          │
│  │ (host fn)│ ◄──func call──  │ (host import) │          │
│  └──────────┘                  └──────────────┘          │
│       │                              │                   │
│       └──── 共享线性内存 (零拷贝) ────┘                   │
│              Linear Memory                               │
│              [0x0000 ... 0xFFFF...]                       │
└──────────────────────────────────────────────────────────┘
```

**函数调用约定：**
- Host → Guest：通过 wasmtime `TypedFunc` 直接调用 guest 导出函数
- Guest → Host：通过 WIT `import` 声明，wasmtime Linker 注入 host function 实现
- 所有调用在同一线程/tokio task 内完成，无线程切换开销
- async host function 通过 wasmtime 的 `async_support` 在 tokio runtime 上 yield

**共享线性内存数据传递：**
- 简单类型（u32, u64, f64, bool）：直接通过函数参数传递，零序列化
- 复合类型（string, list, record）：通过 Component Model 的 Canonical ABI 在线性内存中布局
- Component Model 自动处理内存分配/释放（`cabi_realloc`），host 无需手动管理
- 数据传递路径：线性内存 → Canonical ABI lift/lower → Rust 类型，全在进程内完成

**性能特征：**
| 操作 | 延迟 | 对比 MCP |
|------|------|----------|
| 无参函数调用 | ~100ns | ~1ms (1000x) |
| 带 string 参数调用 | ~1-10μs | ~2-5ms (500x) |
| 带复杂 record 调用 | ~5-20μs | ~5-10ms (250x) |

### 3.2 权限门控机制

每个 host function 调用经过已授予权限检查（详见[第 8 节：权限模型](#8-权限模型)）：

```
Guest 调用 host import
  → wasmtime host function 入口
    → 检查 HostState.granted_permissions
      → 已授权 → 执行
      → 未授权 → 返回 permission-denied 错误
      → 可动态申请 → 触发审批流程 → 等待授予/拒绝
```

权限检查本身是纯内存操作（HashSet lookup），开销 <100ns，不影响调用路径性能。

### 3.3 能力协商协议

插件启动时经过声明-审批-授予流程：

```
┌──────────┐        ┌──────────┐        ┌──────────────┐
│  Plugin  │        │   PRX    │        │ User/Admin   │
│ manifest │        │  Loader  │        │  (审批者)     │
└────┬─────┘        └────┬─────┘        └──────┬───────┘
     │                   │                     │
     │  1. declare       │                     │
     │  permissions:     │                     │
     │  [log,config,     │                     │
     │   http,kv]        │                     │
     │──────────────────►│                     │
     │                   │  2. 审批请求         │
     │                   │  "weather-tool 请求: │
     │                   │   log,config,http,kv"│
     │                   │────────────────────►│
     │                   │                     │
     │                   │  3. grant/deny       │
     │                   │  granted: [log,      │
     │                   │   config,http,kv]    │
     │                   │◄────────────────────│
     │                   │                     │
     │  4. load with     │                     │
     │  granted set      │                     │
     │◄──────────────────│                     │
     │                   │                     │
     │  [运行时]          │                     │
     │  5. request new   │                     │
     │  permission: llm  │                     │
     │──────────────────►│  6. 动态审批         │
     │                   │────────────────────►│
     │                   │  7. grant            │
     │                   │◄────────────────────│
     │  8. granted       │                     │
     │◄──────────────────│                     │
```

**三阶段流程：**
1. **声明阶段（Declare）：** 插件 manifest (`plugin.toml`) 声明所需权限集合
2. **审批阶段（Grant/Deny）：** 加载时由用户/管理员审批，可全部批准、部分批准或拒绝
3. **运行时动态请求（Runtime Request）：** 插件可在运行时请求额外权限，触发动态审批

### 3.4 事件订阅模型

插件通过事件总线实现松耦合的进程内通信：

```
Plugin A                     PRX Event Bus                  Plugin B
   │                              │                            │
   │  subscribe("weather.*")      │                            │
   │─────────────────────────────►│                            │
   │                              │  subscribe("weather.alert")│
   │                              │◄───────────────────────────│
   │                              │                            │
   │  publish("weather.update",   │                            │
   │    {city:"tokyo",temp:25})   │                            │
   │─────────────────────────────►│                            │
   │                              │  on-event("weather.update",│
   │                              │    {city:"tokyo",temp:25}) │
   │                              │───────────────────────────►│
```

**特性：**
- 事件总线在进程内，通过函数调用分发，延迟 <1μs
- 支持通配符订阅（`topic.*`）
- fire-and-forget 语义，不阻塞发布者
- 所有事件经过 host 中转，可审计、可限流

### 3.5 与 MCP 的架构对比

```
┌─────────────────────────────────────────────────────────────┐
│                     对比维度                                 │
├──────────────┬────────────────────┬──────────────────────────┤
│              │   WASM Plugin      │      MCP                 │
│              │   (进程级 ABI)     │   (网络级协议)            │
├──────────────┼────────────────────┼──────────────────────────┤
│ 传输层       │ 函数调用           │ stdio / HTTP+SSE         │
│ 编码         │ Canonical ABI      │ JSON-RPC 2.0             │
│              │ (二进制，零拷贝)    │ (文本，完整序列化)        │
│ 延迟         │ 1-10 μs            │ 1-100 ms                 │
│ 部署         │ .wasm 文件热加载    │ 独立进程/远程服务         │
│ 语言支持     │ 编译到 WASM 的语言  │ 任意语言                 │
│ 安全边界     │ WASM 沙箱 + 权限   │ 进程隔离 + 认证          │
│ 状态共享     │ 线性内存（隔离）    │ 无共享状态               │
│ 适用场景     │ 高频工具、中间件    │ 远程服务、跨机器工具      │
│              │ Hook、自定义 Channel│ 第三方 API 集成          │
└──────────────┴────────────────────┴──────────────────────────┘
```

**选择指南：**
- 调用频率 > 100次/秒，或延迟敏感 → WASM Plugin
- 需要访问远程服务，或已有独立进程 → MCP
- 中间件、Hook 等需要拦截 Agent Loop 的扩展 → 只能用 WASM Plugin
- 两者可以共存：WASM 插件内部可通过 `prx:host/http` 调用远程 MCP server

---

## 4. WIT 接口定义

### 4.1 包结构

```
wit/
├── deps/
│   └── wasi/          # WASI Preview 2 标准接口
├── host/
│   ├── browser.wit    # 浏览器控制（navigate, click, screenshot, evaluate）
│   ├── config.wit     # 配置读取
│   ├── database.wit   # 数据库操作（连接池由 host 管理）
│   ├── device.wit     # 远程设备交互（手机 app, IoT）
│   ├── event.wit      # 事件总线（插件间通信，进程级）
│   ├── filesystem.wit # 受控文件系统访问（路径白名单）
│   ├── http.wit       # 受控网络请求（URL 白名单）
│   ├── kv.wit         # 插件专属 KV 存储
│   ├── llm.wit        # LLM 调用
│   ├── log.wit        # 日志
│   ├── memory.wit     # 记忆系统
│   ├── message.wit    # 消息发送
│   └── time.wit       # 时间
├── plugin/
│   ├── tool.wit       # Tool capability
│   ├── channel.wit    # Channel capability
│   ├── provider.wit   # Provider capability
│   ├── middleware.wit  # Middleware capability
│   ├── hook.wit        # Hook capability
│   ├── api.wit         # API endpoint capability
│   ├── storage.wit     # Storage backend capability
│   └── cron.wit        # Cron job capability
└── worlds.wit          # 组合各 world
```

### 4.2 核心 WIT 定义

#### 4.2.1 公共类型

```wit
package prx:types@0.1.0;

interface types {
    /// 操作结果
    record plugin-result {
        success: bool,
        output: string,
        error: option<string>,
    }
    
    /// JSON 值（用 string 传递，各语言自行解析）
    type json = string;
    
    /// 消息
    record channel-message {
        id: string,
        sender: string,
        reply-target: string,
        content: string,
        channel-name: string,
        timestamp: u64,
        thread-id: option<string>,
    }
    
    /// 发送消息
    record send-message {
        content: string,
        recipient: string,
        subject: option<string>,
        thread-id: option<string>,
    }
    
    /// 工具规格
    record tool-spec {
        name: string,
        description: string,
        parameters-schema: json,
    }
    
    /// LLM 消息
    record chat-message {
        role: string,
        content: string,
    }
    
    /// LLM 响应
    record chat-response {
        text: option<string>,
        tool-calls: list<tool-call-request>,
    }
    
    record tool-call-request {
        id: string,
        name: string,
        arguments: json,
    }
    
    /// Hook 事件类型
    enum hook-event {
        agent-start,
        agent-end,
        llm-request,
        llm-response,
        tool-call-start,
        tool-call,
        turn-complete,
        error,
    }
    
    /// 中间件决策
    variant middleware-action {
        continue(string),       // 继续，可能修改内容
        block(string),          // 阻止，返回原因
        redirect(string),       // 重定向到其他处理
    }
    
    /// HTTP 请求/响应
    record http-request {
        method: string,
        path: string,
        headers: list<tuple<string, string>>,
        body: option<list<u8>>,
        query: list<tuple<string, string>>,
    }
    
    record http-response {
        status: u16,
        headers: list<tuple<string, string>>,
        body: list<u8>,
    }
    
    /// Cron 调度
    record cron-context {
        job-id: string,
        job-name: string,
        scheduled-at: u64,
        run-count: u32,
    }
}
```

#### 4.2.2 Host 提供的接口（插件可调用）

```wit
package prx:host@0.1.0;

/// ──────────── 基础设施 ────────────

/// 日志
interface log {
    enum level {
        trace,
        debug,
        info,
        warn,
        error,
    }
    log: func(level: level, message: string);
}

/// 配置
interface config {
    /// 获取插件专属配置值
    get: func(key: string) -> option<string>;
    /// 获取所有配置
    get-all: func() -> list<tuple<string, string>>;
}

/// 时间
interface clock {
    /// 当前 Unix 时间戳（毫秒）
    now-ms: func() -> u64;
    /// 当前时区
    timezone: func() -> string;
}

/// ──────────── 存储 ────────────

/// KV 存储（插件隔离命名空间）
interface kv {
    get: func(key: string) -> option<list<u8>>;
    set: func(key: string, value: list<u8>) -> result<_, string>;
    delete: func(key: string) -> result<bool, string>;
    list-keys: func(prefix: string) -> list<string>;
}

/// 数据库操作（连接池由 host 管理）
interface database {
    record query-result {
        columns: list<string>,
        rows: list<list<string>>,
        rows-affected: u64,
    }
    
    /// 执行 SQL 查询（只读）
    query: func(sql: string, params: list<string>) -> result<query-result, string>;
    
    /// 执行 SQL 语句（写入）
    execute: func(sql: string, params: list<string>) -> result<u64, string>;
}

/// 受控文件系统访问（路径白名单）
interface filesystem {
    /// 读取文件内容（路径必须在白名单内）
    read-file: func(path: string) -> result<list<u8>, string>;
    
    /// 写入文件（路径必须在白名单内）
    write-file: func(path: string, content: list<u8>) -> result<_, string>;
    
    /// 列出目录内容
    list-dir: func(path: string) -> result<list<string>, string>;
    
    /// 检查文件是否存在
    exists: func(path: string) -> bool;
}

/// ──────────── 通信 ────────────

/// 受控 HTTP 出站（URL 白名单）
interface http-outbound {
    use prx:types/types.{http-response};
    
    request: func(
        method: string,
        url: string,
        headers: list<tuple<string, string>>,
        body: option<list<u8>>,
    ) -> result<http-response, string>;
}

/// 消息发送
interface messaging {
    use prx:types/types.{send-message};
    
    /// 通过指定 channel 发送消息
    send: func(channel-name: string, message: send-message) -> result<_, string>;
    
    /// 通过当前活跃 channel 回复
    reply: func(recipient: string, content: string) -> result<_, string>;
}

/// 事件总线（插件间通信，进程级）
interface events {
    /// 发布事件（其他插件可订阅）
    publish: func(topic: string, payload: string) -> result<_, string>;
    
    /// 订阅事件（注册回调，通过 guest export 接收）
    subscribe: func(topic: string) -> result<u64, string>;
    
    /// 取消订阅
    unsubscribe: func(subscription-id: u64) -> result<_, string>;
}

/// ──────────── AI ────────────

/// LLM 调用
interface llm {
    use prx:types/types.{chat-message, chat-response, json};
    
    /// 调用默认 LLM
    chat: func(
        messages: list<chat-message>,
        model: option<string>,
        temperature: option<f64>,
    ) -> result<chat-response, string>;
}

/// 记忆系统
interface memory {
    record memory-entry {
        id: string,
        text: string,
        category: string,
        importance: f64,
        created-at: u64,
    }
    
    store: func(text: string, category: string, importance: f64) -> result<string, string>;
    recall: func(query: string, limit: u32) -> result<list<memory-entry>, string>;
    forget: func(id: string) -> result<bool, string>;
}

/// ──────────── 设备与浏览器 ────────────

/// 浏览器控制
interface browser {
    use prx:types/types.{json};
    
    /// 导航到 URL
    navigate: func(url: string) -> result<_, string>;
    
    /// 点击元素
    click: func(selector: string) -> result<_, string>;
    
    /// 截图（返回 PNG 字节）
    screenshot: func() -> result<list<u8>, string>;
    
    /// 获取页面快照（可访问性树）
    snapshot: func() -> result<string, string>;
    
    /// 在页面执行 JavaScript
    evaluate: func(script: string) -> result<json, string>;
    
    /// 输入文本
    type-text: func(selector: string, text: string) -> result<_, string>;
}

/// 远程设备交互（手机 app, IoT）
interface device {
    use prx:types/types.{json};
    
    /// 列出已连接设备
    list-devices: func() -> result<list<string>, string>;
    
    /// 向设备发送命令
    send-command: func(device-id: string, command: string, params: json) -> result<json, string>;
    
    /// 获取设备状态
    get-status: func(device-id: string) -> result<json, string>;
    
    /// 拍照（手机摄像头）
    camera-snap: func(device-id: string, facing: string) -> result<list<u8>, string>;
}
```

#### 4.2.3 Plugin Capability Worlds

```wit
package prx:plugin@0.1.0;

/// ============ Tool Capability ============
world tool {
    // Host 提供
    import prx:host/log;
    import prx:host/config;
    import prx:host/kv;
    import prx:host/http-outbound;
    import prx:host/messaging;
    import prx:host/llm;
    import prx:host/clock;
    import wasi:io/streams@0.2.0;
    import wasi:random/random@0.2.0;
    
    use prx:types/types.{tool-spec, json, plugin-result};
    
    // Guest 导出
    export get-spec: func() -> tool-spec;
    export execute: func(args: json) -> plugin-result;
    
    // 可选：多工具导出
    export get-specs: func() -> list<tool-spec>;
    export execute-named: func(name: string, args: json) -> plugin-result;
}

/// ============ Channel Capability ============
world channel {
    import prx:host/log;
    import prx:host/config;
    import prx:host/kv;
    import prx:host/http-outbound;
    import prx:host/clock;
    
    use prx:types/types.{channel-message, send-message};
    
    export name: func() -> string;
    export send: func(message: send-message) -> result<_, string>;
    export health-check: func() -> bool;
    
    // 事件驱动：host 调用 on-message 推送消息给插件处理
    // 实际 listen 由 host 驱动（HTTP webhook / polling）
    export on-message: func(raw-payload: string) -> option<channel-message>;
}

/// ============ Provider Capability ============
world provider {
    import prx:host/log;
    import prx:host/config;
    import prx:host/http-outbound;
    import prx:host/clock;
    
    use prx:types/types.{chat-message, chat-response, tool-spec, json};
    
    export name: func() -> string;
    export chat: func(
        messages: list<chat-message>,
        tools: option<list<tool-spec>>,
        model: string,
        temperature: f64,
    ) -> result<chat-response, string>;
}

/// ============ Middleware Capability ============
world middleware {
    import prx:host/log;
    import prx:host/config;
    import prx:host/kv;
    import prx:host/memory;
    import prx:host/clock;
    
    use prx:types/types.{middleware-action, channel-message, json};
    
    export name: func() -> string;
    export priority: func() -> u32;
    
    /// 入站消息中间件
    export on-inbound: func(message: channel-message) -> middleware-action;
    
    /// 出站响应中间件
    export on-outbound: func(response: string, recipient: string) -> middleware-action;
    
    /// 工具调用中间件
    export on-tool-call: func(tool-name: string, args: json) -> middleware-action;
}

/// ============ Hook Capability ============
world hook {
    import prx:host/log;
    import prx:host/config;
    import prx:host/kv;
    import prx:host/http-outbound;
    import prx:host/messaging;
    import prx:host/clock;
    
    use prx:types/types.{hook-event, json};
    
    export name: func() -> string;
    
    /// 返回此 hook 监听的事件列表
    export subscribed-events: func() -> list<hook-event>;
    
    /// 事件触发
    export on-event: func(event: hook-event, payload: json);
}

/// ============ API Endpoint Capability ============
world api {
    import prx:host/log;
    import prx:host/config;
    import prx:host/kv;
    import prx:host/http-outbound;
    import prx:host/llm;
    import prx:host/memory;
    import prx:host/clock;
    
    use prx:types/types.{http-request, http-response};
    
    /// 返回 API 路由前缀，如 "/api/plugins/my-plugin"
    export route-prefix: func() -> string;
    
    /// 处理 HTTP 请求
    export handle: func(request: http-request) -> http-response;
}

/// ============ Storage Backend Capability ============
world storage {
    import prx:host/log;
    import prx:host/config;
    import prx:host/clock;
    
    export name: func() -> string;
    
    /// Memory trait 的 WASM 实现
    export store-memory: func(text: string, category: string, importance: f64) -> result<string, string>;
    export recall-memory: func(query: string, limit: u32) -> result<string, string>;
    export forget-memory: func(id: string) -> result<bool, string>;
}

/// ============ Cron Job Capability ============
world cron-job {
    import prx:host/log;
    import prx:host/config;
    import prx:host/kv;
    import prx:host/http-outbound;
    import prx:host/messaging;
    import prx:host/llm;
    import prx:host/clock;
    
    use prx:types/types.{cron-context};
    
    export name: func() -> string;
    
    /// Cron 表达式
    export schedule: func() -> string;
    
    /// 执行
    export run: func(ctx: cron-context) -> result<string, string>;
}
```

---

## 5. Capability 详细设计

### 5.1 Tool Capability

**映射关系：** `prx:plugin/tool` world ↔ PRX `Tool` trait

| PRX Tool trait 方法 | WIT export | 说明 |
|---------------------|------------|------|
| `name()` | 通过 `get-spec().name` | |
| `description()` | 通过 `get-spec().description` | |
| `parameters_schema()` | 通过 `get-spec().parameters-schema` | JSON string |
| `execute(args)` | `execute(args)` | async → wasmtime async call |
| `specs()` | `get-specs()` | 多工具导出 |
| `execute_named(name, args)` | `execute-named(name, args)` | |

**Host 端适配器：**

```rust
// 概念代码，不是实际实现
struct WasmTool {
    instance: Arc<Mutex<PluginInstance>>,
    cached_specs: Vec<ToolSpec>,
}

#[async_trait]
impl Tool for WasmTool {
    fn name(&self) -> &str { &self.cached_specs[0].name }
    
    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let mut inst = self.instance.lock().await;
        // wasmtime async call - 零线程切换
        let result = inst.call_execute(args.to_string()).await?;
        Ok(result.into())
    }
}
```

**关键点：**
- Tool 的 `execute` 是 async 的，wasmtime 的 async call 直接在 tokio runtime 上 yield，零开销
- `get-spec()` 在加载时调用一次并缓存，不会每次 LLM turn 都调

### 5.2 Channel Capability

**设计考量：** Channel 的 `listen()` 是长运行的，WASM 不适合做长连接。

**方案：** Host 驱动模式
- 对于 webhook-based channel（如 Telegram Bot API、Slack Events API）：host 负责 HTTP server 接收 webhook，调用插件的 `on-message()` 解析
- 对于 polling-based channel：host 负责 polling，插件只做消息解析
- 对于 WebSocket channel：host 负责 WS 连接，插件做协议编解码

```
Webhook → PRX Gateway → channel plugin.on-message(raw) → ChannelMessage
                  ↓
    channel plugin.send(message) → HTTP outbound (host function)
```

### 5.3 Provider Capability

**映射：** `prx:plugin/provider` ↔ PRX `Provider` trait

- 允许用户用任何语言实现自定义 LLM Provider
- HTTP 请求通过 host function（`http-outbound`）发出，不直接暴露 socket
- 适合接入非标准 API（自建模型、私有部署等）

### 5.4 Middleware Capability

**新增能力**，PRX 现有架构没有 middleware trait，这是插件系统的增值。

处理管线：

```
用户消息
  → middleware[0].on-inbound() → continue/block
  → middleware[1].on-inbound() → continue/block
  → ... (按 priority 排序)
  → Agent Loop 处理
  → middleware[N].on-outbound() → continue/block
  → 发送回复
```

**用例：**
- 内容审核（检测敏感词、NSFW）
- 速率限制（per-user）
- 翻译中间件
- 日志审计

### 5.5 Hook Capability

**映射：** `prx:plugin/hook` ↔ PRX `HookManager`

现有 HookManager 支持两种执行器类型，WASM hook 是第二种：
- `type=command`：现有的 JSON 配置 + 外部命令执行器
- `type=wasm`：WASM 插件执行器，同进程运行

两者共存于同一个 HookManager，不冲突。WASM hook 的优势：
- 更低延迟（函数调用 vs fork/exec）
- 沙箱隔离
- 可访问 host function（如 messaging、kv）

### 5.6 API Endpoint Capability

**新增能力**，允许插件注册自定义 HTTP 端点到 PRX Gateway。

```
GET /api/plugins/weather/forecast?city=tokyo
  → PRX Gateway router
    → weather-plugin.handle(request)
      → plugin 内部调用 http-outbound 获取天气数据
    → http-response
```

路由挂载在 `/api/plugins/{plugin-name}/` 下，与 PRX 内置 API 隔离。

### 5.7 Storage Backend Capability

允许用户实现自定义 Memory backend（如 Pinecone、Qdrant、自建向量库）。

### 5.8 Cron Job Capability

**映射：** 与 PRX 现有 cron 系统集成。

- 插件声明 `schedule()` 返回 cron 表达式
- PRX cron engine 按调度调用 `run()`
- 插件内可访问所有 host function

---

## 6. Host Functions 完整清单

### 6.1 基础设施

| 接口 | 函数 | async | 说明 |
|------|------|-------|------|
| `log` | `log(level, msg)` | 否 | 结构化日志 |
| `config` | `get(key)` | 否 | 读插件配置 |
| `config` | `get-all()` | 否 | 读所有配置 |
| `clock` | `now-ms()` | 否 | 当前时间戳 |
| `clock` | `timezone()` | 否 | 当前时区 |

### 6.2 存储

| 接口 | 函数 | async | 说明 |
|------|------|-------|------|
| `kv` | `get(key)` | **是** | KV 读 |
| `kv` | `set(key, value)` | **是** | KV 写 |
| `kv` | `delete(key)` | **是** | KV 删 |
| `kv` | `list-keys(prefix)` | **是** | KV 列举 |
| `database` | `query(sql, params)` | **是** | SQL 查询（只读） |
| `database` | `execute(sql, params)` | **是** | SQL 执行（写入） |
| `filesystem` | `read-file(path)` | **是** | 读文件（路径白名单） |
| `filesystem` | `write-file(path, content)` | **是** | 写文件（路径白名单） |
| `filesystem` | `list-dir(path)` | **是** | 列目录 |
| `filesystem` | `exists(path)` | 否 | 检查文件存在 |

### 6.3 通信

| 接口 | 函数 | async | 说明 |
|------|------|-------|------|
| `messaging` | `send(channel, msg)` | **是** | 发消息 |
| `messaging` | `reply(recipient, content)` | **是** | 快捷回复 |
| `http-outbound` | `request(method, url, headers, body)` | **是** | HTTP 请求（URL 白名单） |

### 6.4 AI

| 接口 | 函数 | async | 说明 |
|------|------|-------|------|
| `llm` | `chat(messages, model, temp)` | **是** | 调用 LLM |
| `memory` | `store(text, category, importance)` | **是** | 存记忆 |
| `memory` | `recall(query, limit)` | **是** | 召回记忆 |
| `memory` | `forget(id)` | **是** | 遗忘 |

### 6.5 事件

| 接口 | 函数 | async | 说明 |
|------|------|-------|------|
| `events` | `publish(topic, payload)` | **是** | 发布事件 |
| `events` | `subscribe(topic)` | **是** | 订阅 |
| `events` | `unsubscribe(id)` | **是** | 取消订阅 |

### 6.6 设备与浏览器

| 接口 | 函数 | async | 说明 |
|------|------|-------|------|
| `browser` | `navigate(url)` | **是** | 导航到 URL |
| `browser` | `click(selector)` | **是** | 点击元素 |
| `browser` | `screenshot()` | **是** | 截图 |
| `browser` | `snapshot()` | **是** | 页面快照 |
| `browser` | `evaluate(script)` | **是** | 执行 JS |
| `browser` | `type-text(selector, text)` | **是** | 输入文本 |
| `device` | `list-devices()` | **是** | 列出设备 |
| `device` | `send-command(id, cmd, params)` | **是** | 发送设备命令 |
| `device` | `get-status(id)` | **是** | 获取设备状态 |
| `device` | `camera-snap(id, facing)` | **是** | 拍照 |

### 6.7 WASI Preview 2 标准接口

| 接口 | 用途 |
|------|------|
| `wasi:io/streams` | 流式 I/O |
| `wasi:random/random` | 随机数生成 |
| `wasi:clocks/wall-clock` | 墙钟时间 |
| `wasi:clocks/monotonic-clock` | 单调时钟 |

**不提供的 WASI 接口：**
- `wasi:filesystem/*` — 通过 `prx:host/filesystem` 代理（路径白名单控制）
- `wasi:sockets/*` — 通过 `prx:host/http-outbound` 代理（URL 白名单控制）
- `wasi:cli/*` — 无 stdin/stdout/env 直接访问

---

## 7. 多语言插件开发方案

### 7.1 Rust

**工具链：** `cargo-component`（Bytecode Alliance 官方）

```bash
cargo install cargo-component
cargo component new my-tool --lib
```

**开发流程：**
1. `cargo component new` 创建项目，自动引入 WIT
2. `cargo component build --release` 编译为 `.wasm` component
3. 将 `.wasm` 放入 PRX 插件目录

**示例（Tool 插件）：**
```rust
// src/lib.rs - 概念示例
wit_bindgen::generate!({
    world: "tool",
    path: "wit",
});

struct MyTool;

impl Guest for MyTool {
    fn get_spec() -> ToolSpec {
        ToolSpec {
            name: "my-tool".into(),
            description: "A custom tool".into(),
            parameters_schema: r#"{"type":"object","properties":{"query":{"type":"string"}}}"#.into(),
        }
    }
    
    fn execute(args: String) -> PluginResult {
        // 可以调用 host function
        prx::host::log::log(Level::Info, "executing my-tool");
        let result = prx::host::http_outbound::request("GET", "https://api.example.com", &[], None);
        // ...
    }
}
export!(MyTool);
```

**成熟度：** ★★★★★ — 原生支持，零开销，最佳性能

### 7.2 Python

**工具链：** `componentize-py`（Bytecode Alliance 官方）

```bash
pip install componentize-py
```

**开发流程：**
1. 生成绑定：`componentize-py -d wit/ -w tool bindings my_tool_bindings`
2. 编写 Python 实现
3. 编译：`componentize-py -d wit/ -w tool componentize --stub-wasi app -o my_tool.wasm`

**示例：**
```python
# app.py
import wit_world

class WitWorld(wit_world.WitWorld):
    def get_spec(self):
        return wit_world.ToolSpec(
            name="py-tool",
            description="A Python tool",
            parameters_schema='{"type":"object"}'
        )
    
    def execute(self, args: str) -> wit_world.PluginResult:
        # 调用 host function
        wit_world.log(wit_world.Level.INFO, "executing py-tool")
        return wit_world.PluginResult(success=True, output="done", error=None)
```

**限制：**
- CPython 解释器嵌入 WASM，二进制约 30-50MB
- 启动时间较慢（冷启动 ~500ms-1s），可通过预实例化缓解
- 不支持 C 扩展（纯 Python only）
- 运行时性能 ~10-50x 慢于 Rust

**成熟度：** ★★★☆☆ — 可用但有限制，持续改进中

### 7.3 JavaScript / TypeScript

**工具链：** `jco` + `componentize-js`（Bytecode Alliance 官方）

```bash
npm install -g @bytecodealliance/jco
```

**开发流程：**
1. 编写 JS/TS 实现
2. 编译：`jco componentize app.js --wit wit/ --world-name tool -o my_tool.wasm`
3. TypeScript 用户先 `tsc` 编译为 JS，再 componentize

**示例：**
```javascript
// app.js
export function getSpec() {
    return {
        name: "js-tool",
        description: "A JavaScript tool",
        parametersSchema: '{"type":"object"}'
    };
}

export function execute(args) {
    // 自动绑定 host function
    const { log } = imports['prx:host/log'];
    log('info', 'executing js-tool');
    return { success: true, output: "done", error: null };
}
```

**限制：**
- 使用 StarlingMonkey（SpiderMonkey-based），二进制约 10-20MB
- 冷启动 ~200-500ms
- 不支持 Node.js 特有 API（fs, net 等）

**成熟度：** ★★★☆☆ — 功能可用，生态逐步完善

### 7.4 Go

**工具链：** TinyGo v0.34+ + `wit-bindgen-go`

```bash
# 安装 TinyGo
# 安装 wit-bindgen-go
go get -tool go.bytecodealliance.org/cmd/wit-bindgen-go
```

**开发流程：**
1. 生成绑定：`go tool wit-bindgen-go generate --world tool --out internal ./wit/`
2. 编写 Go 实现
3. 编译：`tinygo build -target=wasip2 -o my_tool.wasm .`

**示例：**
```go
package main

import (
    "example.com/internal/prx/types"
)

//go:generate go tool wit-bindgen-go generate --world tool --out internal ./wit/

type MyTool struct{}

func (t *MyTool) GetSpec() types.ToolSpec {
    return types.ToolSpec{
        Name:             "go-tool",
        Description:      "A Go tool",
        ParametersSchema: `{"type":"object"}`,
    }
}

func (t *MyTool) Execute(args string) types.PluginResult {
    return types.PluginResult{
        Success: true,
        Output:  "done",
    }
}

func init() {
    types.SetExports(&MyTool{})
}

func main() {}
```

**限制：**
- 必须使用 TinyGo（不是标准 Go），部分标准库不可用
- 二进制约 2-5MB
- 不支持 goroutine（TinyGo WASM 限制）
- 反射支持有限

**成熟度：** ★★★★☆ — TinyGo 对 WASI P2 支持较好

### 7.5 语言对比总结

| 维度 | Rust | Python | JavaScript | Go |
|------|------|--------|------------|-----|
| 二进制大小 | ~100KB-1MB | ~30-50MB | ~10-20MB | ~2-5MB |
| 冷启动 | <1ms | 500ms-1s | 200-500ms | ~5ms |
| 运行时性能 | 原生 | 10-50x 慢 | 5-20x 慢 | 2-5x 慢 |
| 标准库支持 | 完整 | 纯 Python | 无 Node API | TinyGo 子集 |
| 开发体验 | 学习曲线陡 | 最简单 | 简单 | 中等 |
| 工具链成熟度 | ★★★★★ | ★★★☆☆ | ★★★☆☆ | ★★★★☆ |
| 推荐场景 | 性能敏感插件 | 快速原型/AI | 前端开发者 | 后端开发者 |

---

## 8. 权限模型

### 8.1 设计理念：申请-授权模式

权限模型采用**声明式权限 + 动态授予**，类似 Android/iOS 的权限模型：

- 不是"全拒绝"（零信任），也不是"全放开"（全信任）
- 插件 manifest 声明所需权限
- 加载时由用户/管理员审批
- 运行时可动态请求新权限，触发审批
- 每个 host function 调用经过已授予权限检查

```
┌─────────────────────────────────────────────────────────┐
│                   权限生命周期                            │
│                                                         │
│  声明 (Declare)                                         │
│  └─ plugin.toml 中列出所需权限                           │
│                                                         │
│  审批 (Approve)                                         │
│  └─ 加载时用户/管理员审核权限请求                         │
│     ├─ 全部批准 → 插件以完整权限运行                      │
│     ├─ 部分批准 → 插件以受限权限运行                      │
│     └─ 拒绝 → 插件不加载                                │
│                                                         │
│  运行时 (Runtime)                                       │
│  └─ 插件可动态请求额外权限                               │
│     └─ 触发用户审批弹窗 → grant/deny                     │
│                                                         │
│  检查 (Check)                                           │
│  └─ 每个 host function 调用前检查 granted_permissions    │
│     ├─ 已授权 → 执行                                    │
│     └─ 未授权 → 返回 permission-denied 错误              │
└─────────────────────────────────────────────────────────┘
```

### 8.2 权限声明（plugin.toml）

```toml
# plugin.toml - 插件清单文件
[plugin]
name = "weather-tool"
version = "1.0.0"
capability = "tool"
author = "community"

[permissions]
# 声明需要的 host function 接口（加载时审批）
required = ["log", "config", "kv", "http-outbound", "clock"]

# 可选权限（运行时按需动态申请）
optional = ["messaging", "llm"]

# HTTP 出站白名单（限定 http-outbound 的访问范围）
http_allowlist = [
    "https://api.openweathermap.org/*",
    "https://wttr.in/*",
]

# 文件系统白名单（限定 filesystem 的访问路径）
# filesystem_allowlist = ["/tmp/plugin-data/*"]

# 数据库权限
# database_tables = ["weather_cache"]  # 允许访问的表

[resources]
# 执行限制
max_fuel = 1_000_000_000          # wasmtime fuel 上限
max_memory_mb = 64                 # 线性内存上限
max_execution_time_ms = 30_000     # 单次调用超时
max_http_requests_per_call = 10    # 单次调用最大 HTTP 请求数
max_kv_storage_mb = 10             # KV 存储上限
```

### 8.3 权限分类

| 权限组 | 包含接口 | 风险等级 | 说明 |
|--------|----------|----------|------|
| 基础 | `log`, `config`, `clock` | 低 | 只读，无副作用 |
| 存储 | `kv` | 低 | 命名空间隔离 |
| 网络 | `http-outbound` | 中 | URL 白名单限制 |
| 文件 | `filesystem` | 中 | 路径白名单限制 |
| 数据库 | `database` | 中-高 | 表级权限控制 |
| 消息 | `messaging` | 高 | 可以代替 agent 发消息 |
| AI | `llm`, `memory` | 高 | 消耗 API 额度 |
| 浏览器 | `browser` | 高 | 可操控浏览器 |
| 设备 | `device` | 高 | 可操控远程设备 |
| 事件 | `events` | 中 | 插件间通信 |

### 8.4 运行时权限检查

```rust
// 概念代码
impl HostState {
    fn check_permission(&self, interface: &str) -> Result<(), PermissionError> {
        if self.granted_permissions.contains(interface) {
            Ok(())
        } else if self.optional_permissions.contains(interface) {
            // 触发动态审批流程
            Err(PermissionError::NeedsApproval(interface.to_string()))
        } else {
            Err(PermissionError::Denied(interface.to_string()))
        }
    }
}

// 每个 host function 入口
fn host_http_request(&mut self, method: &str, url: &str, ...) -> Result<Response> {
    self.check_permission("http-outbound")?;
    self.check_url_allowlist(url)?;
    // ... 执行请求
}
```

### 8.5 沙箱机制

| 隔离维度 | 机制 |
|----------|------|
| 内存隔离 | WASM 线性内存，天然隔离 |
| CPU 限制 | wasmtime fuel 计量 + epoch-based 中断 |
| 时间限制 | `tokio::time::timeout` 包裹每次调用 |
| 网络隔离 | 通过 `http-outbound` 代理 + URL 白名单 |
| 文件隔离 | 通过 `filesystem` 代理 + 路径白名单 |
| KV 隔离 | 命名空间隔离，每个插件只能访问自己的 namespace |
| 数据库隔离 | 连接池由 host 管理 + 表级权限控制 |
| 密钥隔离 | 插件配置通过 host function 注入，不暴露全局 config |

### 8.6 审计与异常处理

- 插件加载时验证 WIT 接口兼容性
- 记录所有 host function 调用（可配置 audit log level）
- 权限拒绝事件记录到审计日志
- 异常隔离：单个插件 panic 不影响 host 或其他插件
- 资源超限自动终止插件实例

---

## 9. 性能分析

### 9.1 调用开销

| 操作 | 预期延迟 |
|------|----------|
| Host → Guest 函数调用（无参数） | ~100ns-1μs |
| Host → Guest（带 string 序列化） | ~1-10μs |
| Guest → Host function（sync，如 log） | ~100ns-500ns |
| Guest → Host function（async，如 http） | ~1μs + 实际 IO 时间 |
| 冷实例化（Rust 插件） | ~1-5ms |
| 冷实例化（预编译 Rust 插件） | <1ms |
| 冷实例化（Python 插件） | ~500ms-1s |

### 9.2 与现有架构对比

| 对比项 | 原生 Rust Tool | WASM Tool | 开销比 |
|--------|---------------|-----------|--------|
| `execute()` 调用 | ~0ns（直接调用） | ~5-10μs | 可忽略（IO bound） |
| 内存占用 | 共享进程内存 | 隔离线性内存（默认 1MB） | 略高 |
| 编译时间 | 影响 PRX 编译 | 独立编译 | 更快（解耦） |

**结论：** WASM 调用开销（<10μs）相对于 LLM API 调用（100ms-10s）可以忽略不计。不是瓶颈。

### 9.3 优化策略

1. **预编译缓存** — `Engine::precompile_component()` 生成平台特定 native code，启动时直接加载
2. **实例池** — 对高频调用的插件维护 warm instance pool
3. **Fuel 预算** — 给 Tool 插件默认 10^9 fuel（约等于几十亿条 WASM 指令），足够复杂计算
4. **Epoch 中断** — 后台线程每 10ms 推进 epoch，实现非侵入式超时

---

## 10. 热加载设计

### 10.1 文件监控

复用 PRX 现有的 `notify` / `notify-debouncer-mini` 基础设施（已用于 config hotreload）：

```
plugins/
├── weather-tool/
│   ├── plugin.toml      # 清单
│   └── plugin.wasm      # 组件
├── content-filter/
│   ├── plugin.toml
│   └── plugin.wasm
└── ...
```

### 10.2 热加载流程

```
文件变更检测 (notify watcher)
  → debounce (500ms)
    → 读取新 plugin.toml + plugin.wasm
      → 验证 WIT 接口兼容性
        → 编译 Component (后台线程)
          → 创建新 PluginInstance
            → 原子替换 Registry 中的旧实例
              → 优雅关闭旧实例（drain 进行中的调用）
                → 完成
```

### 10.3 版本兼容性

- WIT 接口版本化：`prx:plugin@0.1.0`
- 加载时检查插件使用的 WIT 版本是否与 host 兼容
- 主版本不兼容 → 拒绝加载并报错
- 次版本向前兼容 → host 提供 shim

---

## 11. 插件间通信

### 11.1 事件总线（Event Bus）

```
Plugin A                    PRX Host                    Plugin B
   │                           │                           │
   │  publish("weather",       │                           │
   │    "tokyo:sunny")         │                           │
   │ ─────────────────────────►│                           │
   │                           │  on-event("weather",      │
   │                           │    "tokyo:sunny")         │
   │                           │──────────────────────────►│
   │                           │                           │
```

### 11.2 实现机制

- Host 端维护 topic → subscriber 映射表
- `publish()` 是 async host function，遍历订阅者依次调用 guest export
- 支持通配符订阅：`weather.*`
- 事件是 fire-and-forget，不支持请求-响应模式
- 如需 RPC 语义，用两个 topic 模拟（request/response）

### 11.3 限制

- 插件间不能直接调用彼此的函数
- 所有通信通过 host 中转（安全可审计）
- 事件 payload 限制 64KB

---

## 12. PDK/SDK 规划

### 12.1 PDK 架构

```
prx-pdk/
├── wit/                    # WIT 定义（所有语言共享）
│   ├── host/
│   ├── plugin/
│   └── worlds.wit
├── rust/                   # Rust PDK
│   ├── prx-pdk/           # crate
│   ├── templates/          # cargo-component 模板
│   └── examples/
├── python/                 # Python PDK
│   ├── prx_pdk/           # pip package
│   ├── templates/
│   └── examples/
├── javascript/             # JS/TS PDK
│   ├── @prx/pdk/          # npm package
│   ├── templates/
│   └── examples/
├── go/                     # Go PDK
│   ├── prx-pdk/           # Go module
│   ├── templates/
│   └── examples/
└── cli/                    # prx-plugin CLI 工具
    ├── new                 # 创建新插件项目
    ├── build               # 构建插件
    ├── validate            # 验证插件
    └── test                # 本地测试
```

### 12.2 `prx-plugin` CLI 工具

```bash
# 创建新插件
prx-plugin new my-tool --lang rust --capability tool
prx-plugin new content-filter --lang python --capability middleware

# 构建
prx-plugin build                     # 检测语言自动构建
prx-plugin build --target release    # release 构建

# 验证
prx-plugin validate plugin.wasm     # 检查 WIT 兼容性、权限声明

# 本地测试（启动 mock host）
prx-plugin test                     # 运行测试用例

# 打包发布
prx-plugin pack                     # 生成 .prxplugin（包含 wasm + manifest）
```

### 12.3 各语言 PDK 内容

**Rust PDK (`prx-pdk` crate):**
- 导出 `wit_bindgen::generate!` 包装宏
- 提供 derive macro 简化 trait 实现
- 类型安全的 host function wrapper
- `#[prx_tool]` / `#[prx_hook]` 属性宏

**Python PDK (`prx_pdk` pip package):**
- 预生成的 WIT bindings
- 基类/装饰器简化实现
- 内置测试 harness

**JavaScript PDK (`@prx/pdk` npm):**
- TypeScript 类型定义
- 构建脚本（包装 jco）
- 测试工具

**Go PDK (`prx-pdk` module):**
- 预生成的 Go bindings
- 构建脚本（包装 tinygo + wasm-tools）

---

## 13. 分期实现计划

### Phase 1: 基础框架（4-5 周）

**目标：** 最小可用插件系统，支持 Rust Tool 插件

| 任务 | 工时 | 说明 |
|------|------|------|
| WIT 接口定义（types + host/log + host/config + plugin/tool） | 3d | 核心类型系统 |
| wasmtime 集成（Engine/Store/Linker 管理） | 3d | 异步配置、fuel/epoch |
| HostState + 基础 host functions（log, config, clock） | 2d | |
| Plugin Registry + Loader | 3d | 加载 .wasm、实例化、注册到 tools_registry |
| WasmTool 适配器（impl Tool for WasmTool） | 2d | 桥接到 Agent Loop |
| plugin.toml manifest 解析 | 1d | |
| 集成测试 + 示例 Rust 插件 | 3d | |
| Cargo.toml 依赖更新 + feature gate | 1d | `features = ["wasm-plugins"]` |
| 文档（开发者指南） | 2d | |
| **小计** | **~20d (4 周)** | |

### Phase 2: Host Functions 扩展 + 安全（3-4 周）

**目标：** 完整 host function 集合 + 安全沙箱

| 任务 | 工时 | 说明 |
|------|------|------|
| host/kv 实现（SQLite backend） | 3d | 插件隔离 KV |
| host/http-outbound 实现 + URL 白名单 | 3d | reqwest 桥接 |
| host/messaging 实现 | 2d | 桥接到 Channel trait |
| host/llm 实现 | 2d | 桥接到 Provider trait |
| host/memory 实现 | 2d | 桥接到 Memory trait |
| 权限系统实现 | 3d | manifest 声明 + 运行时检查 |
| 资源限制（fuel/memory/timeout） | 2d | |
| 安全审计日志 | 1d | |
| **小计** | **~18d (3.5 周)** | |

### Phase 3: 多 Capability + 热加载（3-4 周）

**目标：** Hook、Middleware、API、Cron capability + 热加载

| 任务 | 工时 | 说明 |
|------|------|------|
| Hook capability（world 定义 + host 集成） | 3d | 替换/补充 hooks.json |
| Middleware capability + 处理管线 | 4d | 新增，Agent Loop 集成 |
| API endpoint capability + Gateway 路由 | 3d | axum 路由动态注册 |
| Cron capability + cron engine 集成 | 2d | |
| Channel capability（webhook 模式） | 3d | |
| 热加载（file watcher + 优雅替换） | 3d | 复用 notify |
| 预编译缓存 | 2d | |
| **小计** | **~20d (4 周)** | |

### Phase 4: 多语言 PDK + 事件总线（4-5 周）

**目标：** Python/JS/Go PDK + 插件间通信

| 任务 | 工时 | 说明 |
|------|------|------|
| Rust PDK（crate + 模板 + 示例） | 3d | |
| Python PDK（pip + 模板 + 示例） | 5d | componentize-py 集成 |
| JavaScript PDK（npm + 模板 + 示例） | 5d | jco 集成 |
| Go PDK（module + 模板 + 示例） | 4d | TinyGo 集成 |
| 事件总线（host/events + 订阅/发布） | 3d | |
| `prx-plugin` CLI 工具 | 3d | new/build/validate/test |
| **小计** | **~23d (4.5 周)** | |

### Phase 5: Storage/Provider Capability + 生产加固（2-3 周）

| 任务 | 工时 | 说明 |
|------|------|------|
| Provider capability | 3d | |
| Storage capability | 3d | |
| 实例池 + 性能优化 | 3d | |
| 文档完善（用户指南 + API 参考） | 3d | |
| 端到端测试 + 压力测试 | 3d | |
| **小计** | **~15d (3 周)** | |

### 总计

| Phase | 工时 | 累计 |
|-------|------|------|
| P1 基础框架 | ~4 周 | 4 周 |
| P2 Host Functions + 安全 | ~3.5 周 | 7.5 周 |
| P3 多 Capability + 热加载 | ~4 周 | 11.5 周 |
| P4 多语言 PDK + 事件 | ~4.5 周 | 16 周 |
| P5 生产加固 | ~3 周 | 19 周 |

**总计约 19 周（~5 个月）** 完成全部功能。P1 完成后即可开始接受社区 Rust 插件。

---

## 14. 风险清单

### 14.1 技术风险

| 风险 | 影响 | 概率 | 缓解 |
|------|------|------|------|
| **componentize-py 稳定性** — Python WASM 编译仍在快速迭代，API 可能变更 | 高 | 中 | Pin 版本；P4 才引入，给上游时间稳定；维护 version matrix |
| **componentize-js / StarlingMonkey 内存占用** — JS 引擎嵌入 WASM 体积大 | 中 | 高 | 文档明确标注二进制大小；提供 lazy load |
| **TinyGo 标准库缺失** — 部分 Go 标准库在 TinyGo 不可用 | 中 | 中 | PDK 文档列出可用/不可用包；提供替代方案 |
| **WIT 规范变更** — Component Model 仍在演进 | 高 | 低 | 跟踪 Bytecode Alliance 路线图；版本化接口 |
| **wasmtime 大版本升级破坏兼容** — wasmtime API 变更频繁 | 中 | 中 | Pin wasmtime 大版本；feature gate 隔离 |
| **async host function 中的死锁** — 复杂调用链可能产生 | 高 | 低 | Store 单线程所有权模型天然防止；code review |

### 14.2 架构风险

| 风险 | 影响 | 概率 | 缓解 |
|------|------|------|------|
| **Channel 长连接模式** — WASM 不适合做 WebSocket 长连接 | 中 | 确定 | Host 驱动模式，插件只做编解码，不做连接管理 |
| **Provider streaming** — Component Model 不原生支持流式 | 中 | 高 | 分块调用模式：host 分段推送给 guest |
| **插件间通信延迟** — 所有 IPC 经过 host 中转 | 低 | 确定 | 可接受；安全 > 性能；host 内存中转，延迟 <1μs |
| **Plugin Registry 并发** — 热加载期间的 race condition | 中 | 中 | Arc + RwLock；优雅 drain 机制 |

### 14.3 生态风险

| 风险 | 影响 | 概率 | 缓解 |
|------|------|------|------|
| **Component Model 生态碎片化** — 各语言工具链不统一 | 中 | 中 | 统一 WIT 定义；PDK 屏蔽工具链差异 |
| **社区学习曲线** — WIT + Component Model 概念较新 | 高 | 高 | 详细文档 + 模板 + CLI 工具降低门槛 |
| **二进制大小** — Python/JS 插件体积大，影响分发 | 中 | 确定 | 分语言优化；支持预编译缓存减少启动开销 |

### 14.4 兼容性风险

| 风险 | 影响 | 概率 | 缓解 |
|------|------|------|------|
| **AppState 膨胀** — 新增 Plugin Registry 到全局状态 | 低 | 确定 | 单独的 PluginManager，AppState 只持有 Arc 引用 |
| **编译时间增加** — wasmtime 依赖树较大 | 中 | 确定 | feature gate `wasm-plugins`，默认不编译；预编译 CI |

---

## 附录 A: Cargo.toml 依赖变更

```toml
[dependencies]
# WASM Plugin System (optional)
wasmtime = { version = "31", optional = true, default-features = false, features = [
    "async", "component-model", "cranelift", "cache"
] }
wasmtime-wasi = { version = "31", optional = true }

[features]
wasm-plugins = ["dep:wasmtime", "dep:wasmtime-wasi"]
```

预估增加编译时间：~30-60s（首次），增加二进制大小 ~5-10MB。

## 附录 B: 插件目录结构

```
~/.config/openprx/plugins/
├── enabled/                    # 已启用的插件（symlink 或直接存放）
│   ├── weather-tool/
│   │   ├── plugin.toml
│   │   └── plugin.wasm
│   └── content-filter/
│       ├── plugin.toml
│       └── plugin.wasm
├── disabled/                   # 已安装但未启用
├── cache/                      # 预编译缓存
│   ├── weather-tool.cwasm      # 平台特定预编译
│   └── content-filter.cwasm
└── data/                       # 插件数据（KV 存储等）
    ├── weather-tool/
    └── content-filter/
```

## 附录 C: plugin.toml 完整示例

```toml
[plugin]
name = "weather-tool"
version = "1.2.0"
description = "Get weather forecasts for any location"
author = "PRX Community"
license = "MIT"
capability = "tool"
wit_version = "0.1.0"
min_prx_version = "0.2.0"

[permissions]
# 必需权限（加载时审批）
required = ["log", "config", "kv", "http-outbound", "clock"]

# 可选权限（运行时按需动态申请）
optional = ["messaging", "llm"]

# HTTP 出站白名单
http_allowlist = [
    "https://api.openweathermap.org/*",
    "https://wttr.in/*",
]

[resources]
max_fuel = 1_000_000_000
max_memory_mb = 64
max_execution_time_ms = 30_000
max_http_requests_per_call = 10
max_kv_storage_mb = 10

[config]
# 插件自定义配置，通过 host/config 接口读取
api_key = "${OPENWEATHER_API_KEY}"
default_units = "metric"
cache_ttl_seconds = "300"
```

---

*文档结束。此文档作为 PRX WASM 插件系统的技术规格，供架构评审和实现参考。*
