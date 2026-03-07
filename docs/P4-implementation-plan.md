# PRX WASM Plugin System — P4 详细实施计划

## 多语言 PDK + 事件总线

**状态：** 规划完成  
**前置条件：** P1-P3 全部完成 ✅  
**预估总工时：** 23 工作日（约 4.5 周）

---

## P1-P3 现状总结

### 已实现的内容

| 组件 | 状态 | 关键文件 |
|------|------|----------|
| wasmtime Engine (async + component model) | ✅ | `src/plugins/mod.rs` |
| PluginManager (load/reload/unload) | ✅ | `src/plugins/mod.rs` |
| PluginRegistry (thread-safe, Arc<RwLock>) | ✅ | `src/plugins/registry.rs` |
| PluginManifest (plugin.toml 解析) | ✅ | `src/plugins/manifest.rs` |
| HostState (per-instance, 权限检查) | ✅ | `src/plugins/host.rs` |
| WIT 定义 (host: log/config/kv/http/memory) | ✅ | `wit/host/*.wit` |
| WIT 定义 (plugin: tool/middleware/hook/cron) | ✅ | `wit/plugin/*.wit` |
| World 定义 (tool/middleware/hook/cron/base) | ✅ | `wit/worlds.wit` |
| Host functions (log/config/kv/http/memory) | ✅ | `src/plugins/capabilities/common.rs`, `tool.rs` |
| WasmToolAdapter (impl Tool trait) | ✅ | `src/plugins/capabilities/tool.rs` |
| WasmMiddleware + MiddlewareChain | ✅ | `src/plugins/capabilities/middleware.rs` |
| WasmHook + WasmHookExecutor | ✅ | `src/plugins/capabilities/hook.rs` |
| WasmCronJob + WasmCronManager + Scheduler | ✅ | `src/plugins/capabilities/cron.rs` |
| HookManager ↔ WasmHookExecutor 集成 | ✅ | `src/hooks/mod.rs` |
| Gateway API (/api/plugins, reload) | ✅ | `src/gateway/api/plugins.rs` |
| Feature gate (wasm-plugins) | ✅ | `Cargo.toml` |
| Example plugin (base64 tool) | ✅ (manifest only) | `plugins/example-base64/` |
| Error types (PluginError) | ✅ | `src/plugins/error.rs` |

### 尚未实现的 Spec 内容

| 组件 | 状态 | Spec 位置 |
|------|------|-----------|
| **Event Bus (host/events)** | ❌ | §11.1-11.3 |
| **Rust PDK (prx-pdk crate)** | ❌ | §12.3 |
| **Python PDK (prx_pdk pip)** | ❌ | §12.3 |
| **JavaScript PDK (@prx/pdk npm)** | ❌ | §12.3 |
| **Go PDK (prx-pdk module)** | ❌ | §12.3 |
| **prx-plugin CLI 工具** | ❌ | §12.2 |
| hot-reload (file watcher) | ❌ | P3 scope, deferred |
| 预编译缓存 | ❌ | P3 scope, deferred |

---

## 实施顺序总览

```
Week 1:  [A1] Event Bus WIT + Host ──┐
         [A2] Event Bus 集成测试     ──┤
                                      ├── Week 2: [B] Rust PDK ──────────┐
Week 1-2:[F1] CLI 基础框架 ──────────┤                                   │
                                      │  Week 2-3: [C] Python PDK ───────┤ (可并行)
                                      │  Week 2-3: [D] JS/TS PDK ────────┤ (可并行)
                                      │  Week 3-4: [E] Go PDK ───────────┤ (可并行)
                                      └── Week 4:  [F2] CLI 完善 ────────┘
                                         Week 4-5: 集成测试 + 文档
```

**关键路径：** A1 → A2 → B → F2（Event Bus 是所有 PDK 的前置，Rust PDK 是其他 PDK 的参考实现）  
**可并行：** C/D/E 之间可并行，与 B 完成后即可开始

---

## A. 事件总线（Event Bus）— 最优先

### A1. WIT 定义 + Host Function 实现

**预估工时：** 2 天  
**依赖：** 无（仅依赖 P1-P3 现有基础）

#### A1.1 新增 WIT 文件

**新增文件：** `wit/host/event.wit`

```wit
/// Event bus interface for inter-plugin communication.
///
/// Provides fire-and-forget publish/subscribe messaging between plugins.
/// All events flow through the host for auditing and access control.
interface events {
    /// Publish an event to a topic.
    /// All subscribers matching the topic will receive the event asynchronously.
    /// Payload is JSON-encoded, max 64KB.
    publish: func(topic: string, payload: string) -> result<_, string>;

    /// Subscribe to a topic pattern.
    /// Supports exact match ("weather.update") and wildcard ("weather.*").
    /// Returns a subscription ID for later unsubscription.
    subscribe: func(topic-pattern: string) -> result<u64, string>;

    /// Cancel a subscription by ID.
    unsubscribe: func(subscription-id: u64) -> result<_, string>;
}
```

**修改文件：** `wit/host/package.wit` — 确认 events interface 在 `prx:host@0.1.0` package 中。

**修改文件：** `wit/worlds.wit` — 在需要事件总线的 world 中添加 import：

```wit
world tool {
    // 现有 imports...
    import prx:host/events;  // 新增
    export prx:plugin/tool-exports;
}

world hook {
    import prx:host/log;
    import prx:host/config;
    import prx:host/kv;
    import prx:host/events;  // 新增
    export prx:plugin/hook-exports;
}
// middleware 和 cron 同理
```

#### A1.2 EventBus 核心数据结构

**新增文件：** `src/plugins/event_bus.rs`

```rust
/// 进程内事件总线
pub struct EventBus {
    /// topic → Vec<Subscription>
    subscriptions: RwLock<HashMap<String, Vec<Subscription>>>,
    /// 全局订阅 ID 计数器
    next_id: AtomicU64,
    /// 通配符订阅 (topic_pattern → Vec<Subscription>)
    wildcard_subscriptions: RwLock<Vec<WildcardSubscription>>,
    /// 审计日志开关
    audit_enabled: bool,
}

struct Subscription {
    id: u64,
    plugin_name: String,
    topic: String,
}

struct WildcardSubscription {
    id: u64,
    plugin_name: String,
    pattern: String,         // e.g. "weather.*"
    prefix: String,          // e.g. "weather."
}
```

**架构决策：**

| 决策点 | 选择 | 理由 |
|--------|------|------|
| 同步 vs 异步分发 | **异步 fire-and-forget** | 不阻塞发布者；spec 明确要求 |
| 订阅者调用方式 | 通过 host→guest `on-event` export | 复用现有 WasmHook 机制 |
| 通配符实现 | 前缀匹配 (`topic.*`) | spec 定义；`*` 只支持尾部通配 |
| payload 大小限制 | **64KB** | spec §11.3 明确要求 |
| 事件分发顺序 | 订阅注册顺序 | 可预测，简单 |
| 错误处理 | 单个订阅者失败不影响其他 | 隔离性 |

#### A1.3 Host Function 注册

**修改文件：** `src/plugins/capabilities/common.rs`

新增 `register_event_host_functions()` 函数，注册到 linker：

- `prx:host/events@0.1.0::publish` — 验证权限("events")，检查 payload 大小(≤64KB)，投递到 EventBus
- `prx:host/events@0.1.0::subscribe` — 验证权限，注册订阅，返回 subscription ID
- `prx:host/events@0.1.0::unsubscribe` — 验证权限，移除订阅

**修改文件：** `src/plugins/host.rs`

在 HostState 中新增：
```rust
pub event_bus: Option<Arc<EventBus>>,
```

**修改文件：** `src/plugins/capabilities/tool.rs`, `hook.rs`, `middleware.rs`, `cron.rs`

在各 adapter 的 `register_host_functions()` 中调用 `register_event_host_functions()`。

#### A1.4 事件分发机制

EventBus 的 `publish()` 实现：

```
publish(topic, payload)
  ├── 检查 payload.len() ≤ 64KB
  ├── 精确匹配: subscriptions[topic] → 收集所有匹配
  ├── 通配符匹配: wildcard_subscriptions.iter().filter(|w| topic.starts_with(&w.prefix))
  ├── 去重 (同一 plugin 不重复接收)
  └── 对每个匹配的 subscriber:
       └── tokio::spawn → 调用该 plugin 的 on-event(topic, payload)
           (通过 WasmHookExecutor 路由，或新建 EventSubscriberExecutor)
```

**重要设计：事件分发不阻塞发布者。** 使用 `tokio::spawn` 异步投递，发布者立即返回。

#### A1.5 与 HookManager 的关系

```
HookManager (现有)                    EventBus (新增)
    │                                      │
    ├── emit(HookEvent, payload)           ├── publish(topic, payload)
    │   ├── hooks.json actions             │   ├── 精确匹配订阅者
    │   └── WasmHookExecutor.emit()        │   ├── 通配符匹配订阅者
    │                                      │   └── 异步调用 on-event
    │                                      │
    └── 生命周期事件                        └── 插件间自定义事件
        (agent_start, tool_call...)            (weather.update, data.sync...)
```

**集成方案：**
- HookManager 的生命周期事件**也**发布到 EventBus（topic 前缀 `prx.lifecycle.*`）
- 插件可以通过 EventBus 订阅生命周期事件，作为 hook 的替代方案
- 两套系统并存，EventBus 是 HookManager 的超集

**修改文件：** `src/hooks/mod.rs`

在 `emit()` 方法尾部新增：将 HookEvent 桥接到 EventBus：
```rust
// 桥接到 EventBus
if let Some(ref bus) = self.event_bus {
    let topic = format!("prx.lifecycle.{}", event.as_str());
    let _ = bus.publish(&topic, &payload.to_string()).await;
}
```

#### A1.6 权限与审计

- 新权限名称：`"events"` — 需在 plugin.toml `[permissions].required` 中声明
- 审计日志：每次 publish/subscribe/unsubscribe 记录 tracing event

**修改文件：** `src/plugins/host.rs`

`check_permission()` 中无需修改（已通用化），但需在文档中说明 `"events"` 是新增的权限接口名。

### A2. EventBus 集成 + 测试

**预估工时：** 1 天  
**依赖：** A1

**新增文件：** `src/plugins/event_bus.rs` 的单元测试模块

测试用例：
1. `publish_to_empty_bus` — 无订阅者时 publish 成功
2. `subscribe_and_receive` — 订阅后收到匹配事件
3. `wildcard_subscribe` — `topic.*` 匹配 `topic.foo` 和 `topic.bar`
4. `unsubscribe` — 取消后不再收到事件
5. `payload_size_limit` — 超过 64KB 返回错误
6. `multiple_subscribers_same_topic` — 多个订阅者都收到
7. `publish_no_cross_topic` — 不匹配的 topic 不会收到

**修改文件：** `src/plugins/mod.rs`

新增 `pub mod event_bus;`

在 PluginManager 中新增：
```rust
pub fn create_event_bus(&self) -> EventBus { ... }
```

**修改文件：** `src/gateway/mod.rs`

在 AppState 中新增 `event_bus: Option<Arc<EventBus>>`，初始化时创建。

**验证方法：**
```bash
cargo test --features wasm-plugins event_bus
cargo test --features wasm-plugins  # 确保不破坏现有测试
cargo test                           # 确保无 feature 时编译通过
```

### A 子任务文件清单

| 操作 | 文件 |
|------|------|
| 新增 | `wit/host/event.wit` |
| 新增 | `src/plugins/event_bus.rs` |
| 修改 | `wit/worlds.wit` (所有 world 添加 events import) |
| 修改 | `src/plugins/mod.rs` (pub mod event_bus) |
| 修改 | `src/plugins/host.rs` (HostState 添加 event_bus 字段) |
| 修改 | `src/plugins/capabilities/common.rs` (register_event_host_functions) |
| 修改 | `src/plugins/capabilities/tool.rs` (调用 event host registration) |
| 修改 | `src/plugins/capabilities/hook.rs` (调用 event host registration) |
| 修改 | `src/plugins/capabilities/middleware.rs` (调用 event host registration) |
| 修改 | `src/plugins/capabilities/cron.rs` (调用 event host registration) |
| 修改 | `src/hooks/mod.rs` (桥接 HookEvent → EventBus) |
| 修改 | `src/gateway/mod.rs` (AppState 添加 event_bus) |

### A 风险点

| 风险 | 影响 | 缓解 |
|------|------|------|
| 异步 on-event 回调需要持有 WASM Store 锁 | 可能死锁 | 用 tokio::spawn 解耦，每个回调独立 task |
| 通配符匹配性能（大量订阅） | O(n) 遍历 | 短期可接受；可后续引入 trie 优化 |
| 循环发布（A publish → B on-event → B publish → A on-event → …） | 栈溢出或资源耗尽 | 添加 depth counter，max 递归深度 = 8 |
| HookManager 桥接增加耦合 | 维护复杂度 | 桥接代码控制在 5 行内，gated behind feature flag |

---

## B. Rust PDK

**预估工时：** 3 天  
**依赖：** A1（EventBus WIT 定义必须确定，因为 PDK 要生成绑定）

### B1. Crate 结构设计

**新增目录：** `pdk/rust/prx-pdk/`

```
pdk/
└── rust/
    └── prx-pdk/
        ├── Cargo.toml
        ├── src/
        │   ├── lib.rs           # re-exports + convenience types
        │   ├── bindings.rs      # wit-bindgen 生成的绑定 (generated)
        │   └── macros.rs        # proc-macro 简化 trait 实现
        ├── prx-pdk-macros/      # proc-macro crate (独立 crate)
        │   ├── Cargo.toml
        │   └── src/lib.rs
        ├── wit/                  # symlink 或复制 ../../wit/
        └── README.md
```

**Cargo.toml 关键依赖：**
```toml
[package]
name = "prx-pdk"
version = "0.1.0"
edition = "2021"
description = "PRX WASM Plugin Development Kit for Rust"
license = "MIT OR Apache-2.0"

[dependencies]
wit-bindgen = "0.42"
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
prx-pdk-macros = { path = "./prx-pdk-macros" }

[lib]
crate-type = ["cdylib", "rlib"]
```

### B2. 宏设计

**`#[prx_tool]` 属性宏：**

```rust
// 用户代码
use prx_pdk::prelude::*;

#[prx_tool(
    name = "weather_lookup",
    description = "Look up weather by city",
    params = r#"{"type":"object","properties":{"city":{"type":"string"}}}"#
)]
fn execute(args: serde_json::Value) -> ToolResult {
    let city = args["city"].as_str().unwrap_or("unknown");
    prx::log::info(&format!("Looking up weather for {city}"));
    ToolResult::ok(format!("Weather for {city}: Sunny, 25°C"))
}
```

**宏展开为：**
```rust
// 自动实现 prx:plugin/tool-exports 的 get-spec 和 execute
struct MyPlugin;
impl prx::plugin::tool_exports::Guest for MyPlugin {
    fn get_spec() -> ToolSpec { ... }
    fn execute(args: String) -> PluginResult { ... }
}
export!(MyPlugin);
```

**`#[prx_hook]` 属性宏：**
```rust
#[prx_hook(events = ["tool_call", "agent_start"])]
fn on_event(event: &str, payload: &str) -> Result<(), String> {
    prx::log::info(&format!("Event: {event}"));
    Ok(())
}
```

**`#[prx_middleware]` 属性宏：**
```rust
#[prx_middleware(priority = 50)]
fn process(stage: &str, data: &str) -> Result<String, String> {
    // transform data
    Ok(data.to_string())
}
```

### B3. Host Function 包装层

`src/lib.rs` 提供人性化的 API：

```rust
pub mod log {
    pub fn info(msg: &str) { /* 调用 wit-bindgen 生成的 prx::host::log::log(...) */ }
    pub fn warn(msg: &str) { ... }
    pub fn error(msg: &str) { ... }
}

pub mod config {
    pub fn get(key: &str) -> Option<String> { ... }
    pub fn get_all() -> Vec<(String, String)> { ... }
}

pub mod kv {
    pub fn get(key: &str) -> Option<Vec<u8>> { ... }
    pub fn set(key: &str, value: &[u8]) -> Result<(), String> { ... }
    pub fn delete(key: &str) -> Result<bool, String> { ... }
    pub fn list_keys(prefix: &str) -> Vec<String> { ... }
}

pub mod events {
    pub fn publish(topic: &str, payload: &str) -> Result<(), String> { ... }
    pub fn subscribe(pattern: &str) -> Result<u64, String> { ... }
    pub fn unsubscribe(id: u64) -> Result<(), String> { ... }
}

pub mod http {
    pub fn request(method: &str, url: &str, headers: &[(&str, &str)], body: Option<&[u8]>) -> Result<HttpResponse, String> { ... }
}
```

### B4. 模板项目

**新增目录：** `pdk/rust/templates/tool/`

```
templates/tool/
├── Cargo.toml.tmpl
├── src/lib.rs.tmpl
├── plugin.toml.tmpl
├── build.sh.tmpl
└── .cargo/config.toml.tmpl
```

### B5. 示例插件

**新增目录：** `pdk/rust/examples/`

**示例 1 — Tool 插件：** `pdk/rust/examples/base64-tool/`
- 完整的 base64 encode/decode tool（替代现有 plugins/example-base64 的参考实现）
- 使用 `#[prx_tool]` 宏

**示例 2 — Hook 插件：** `pdk/rust/examples/audit-hook/`
- 监听 `prx.lifecycle.*` 事件
- 使用 KV 存储记录事件计数
- 使用 `#[prx_hook]` 宏

### B6. 发布准备

- `Cargo.toml` 中设置 `repository`, `documentation`, `keywords`
- 添加 `LICENSE-MIT` 和 `LICENSE-APACHE`
- `README.md` 包含快速入门
- `CHANGELOG.md`

**验证方法：**
```bash
cd pdk/rust/prx-pdk
cargo build
cargo test
cargo doc --no-deps  # 文档生成

cd examples/base64-tool
cargo component build --release
# 将生成的 .wasm 复制到 plugins/ 目录下测试加载
```

### B 文件清单

| 操作 | 文件 |
|------|------|
| 新增 | `pdk/rust/prx-pdk/Cargo.toml` |
| 新增 | `pdk/rust/prx-pdk/src/lib.rs` |
| 新增 | `pdk/rust/prx-pdk/src/macros.rs` |
| 新增 | `pdk/rust/prx-pdk-macros/Cargo.toml` |
| 新增 | `pdk/rust/prx-pdk-macros/src/lib.rs` |
| 新增 | `pdk/rust/templates/tool/*` |
| 新增 | `pdk/rust/templates/hook/*` |
| 新增 | `pdk/rust/templates/middleware/*` |
| 新增 | `pdk/rust/examples/base64-tool/*` |
| 新增 | `pdk/rust/examples/audit-hook/*` |
| 新增 | `pdk/rust/README.md` |

### B 风险点

| 风险 | 影响 | 缓解 |
|------|------|------|
| wit-bindgen 版本不匹配 wasmtime 31 | 编译失败 | wit-bindgen 0.42 对应 wasmtime 31；锁定版本 |
| proc-macro crate 增加复杂度 | 维护成本 | 先实现简单版本（无 proc-macro），后续迭代 |
| cargo-component 必须安装 | 用户体验 | CLI 工具自动检测并提示安装 |

---

## C. Python PDK

**预估工时：** 5 天  
**依赖：** A1（WIT 定义确定），B（参考实现）  
**可并行：** 与 D、E 并行

### C1. 版本兼容性

- **componentize-py:** ≥ 0.16（支持 WASI Preview 2 + Component Model）
- **Python:** ≥ 3.10（componentize-py 要求）
- **wasmtime 兼容:** componentize-py 0.16 生成的组件与 wasmtime 31 兼容

### C2. 包结构

**新增目录：** `pdk/python/`

```
pdk/python/
├── prx_pdk/
│   ├── __init__.py      # re-exports
│   ├── bindings/        # componentize-py 生成的绑定
│   ├── decorators.py    # @prx_tool, @prx_hook, @prx_middleware
│   ├── types.py         # ToolSpec, PluginResult, etc.
│   └── host.py          # 包装层 (log, config, kv, events, http)
├── templates/
│   └── tool/
│       ├── plugin.py.tmpl
│       └── plugin.toml.tmpl
├── examples/
│   ├── hello-tool/
│   │   ├── plugin.py
│   │   ├── plugin.toml
│   │   └── build.sh
│   └── logger-hook/
│       ├── plugin.py
│       ├── plugin.toml
│       └── build.sh
├── pyproject.toml
├── README.md
└── tests/
    └── test_types.py
```

### C3. 装饰器设计

```python
from prx_pdk import prx_tool, ToolResult, log, config

@prx_tool(
    name="json_formatter",
    description="Format JSON with indentation",
    params={
        "type": "object",
        "properties": {
            "json_str": {"type": "string"},
            "indent": {"type": "integer", "default": 2}
        }
    }
)
def execute(args: dict) -> ToolResult:
    import json
    data = json.loads(args["json_str"])
    formatted = json.dumps(data, indent=args.get("indent", 2))
    return ToolResult.ok(formatted)
```

### C4. 绑定生成流程

```bash
# 1. 从 WIT 生成 Python 绑定
componentize-py --wit-path ../../wit bindings pdk/python/prx_pdk/bindings/

# 2. 构建 WASM 组件
componentize-py --wit-path ../../wit --world tool componentize plugin.py -o plugin.wasm
```

### C5. 性能基线

| 指标 | 目标 | 备注 |
|------|------|------|
| 二进制大小 | < 10 MB | componentize-py 包含 Python 解释器 |
| 冷启动 | < 500ms | 首次实例化含 Python 初始化 |
| 热调用延迟 | < 10ms | 后续 execute 调用 |
| 内存占用 | < 50 MB | Python runtime 开销 |

### C 文件清单

| 操作 | 文件 |
|------|------|
| 新增 | `pdk/python/prx_pdk/__init__.py` |
| 新增 | `pdk/python/prx_pdk/decorators.py` |
| 新增 | `pdk/python/prx_pdk/types.py` |
| 新增 | `pdk/python/prx_pdk/host.py` |
| 新增 | `pdk/python/pyproject.toml` |
| 新增 | `pdk/python/templates/tool/*` |
| 新增 | `pdk/python/examples/hello-tool/*` |
| 新增 | `pdk/python/examples/logger-hook/*` |
| 新增 | `pdk/python/README.md` |

### C 风险点

| 风险 | 影响 | 缓解 |
|------|------|------|
| componentize-py 版本迭代快 | API 不稳定 | 锁定版本，CI 测试 |
| Python WASM 二进制大 (~8-15MB) | 加载慢 | 文档说明；预编译缓存 (P5) |
| Python 第三方库不可用 | 功能受限 | 文档说明纯 Python 限制；标准库可用 |
| 调试困难 | 开发体验差 | 提供 mock host 本地测试 |

---

## D. JavaScript/TypeScript PDK

**预估工时：** 5 天  
**依赖：** A1，B  
**可并行：** 与 C、E 并行

### D1. 版本检查

- **jco:** ≥ 1.6（Component Model 支持）
- **componentize-js:** ≥ 0.12
- **Node.js:** ≥ 20（构建时需要）
- **TypeScript:** ≥ 5.0

### D2. 包结构

**新增目录：** `pdk/javascript/`

```
pdk/javascript/
├── packages/
│   └── prx-pdk/
│       ├── package.json         # @prx/pdk
│       ├── tsconfig.json
│       ├── src/
│       │   ├── index.ts         # re-exports
│       │   ├── types.ts         # ToolSpec, PluginResult, etc.
│       │   ├── host.ts          # log, config, kv, events, http wrappers
│       │   └── decorators.ts    # 装饰器 (experimental)
│       └── dist/                # 编译输出
├── templates/
│   └── tool/
│       ├── src/plugin.ts.tmpl
│       ├── package.json.tmpl
│       ├── tsconfig.json.tmpl
│       └── plugin.toml.tmpl
├── examples/
│   ├── markdown-tool/
│   │   ├── src/plugin.ts
│   │   ├── package.json
│   │   ├── plugin.toml
│   │   └── build.sh
│   └── rate-limiter-middleware/
│       ├── src/plugin.ts
│       ├── package.json
│       ├── plugin.toml
│       └── build.sh
└── README.md
```

### D3. TypeScript 类型定义

```typescript
// types.ts
export interface ToolSpec {
  name: string;
  description: string;
  parametersSchema: string; // JSON Schema string
}

export interface PluginResult {
  success: boolean;
  output: string;
  error?: string;
}

export interface HttpResponse {
  status: number;
  headers: [string, string][];
  body: Uint8Array;
}

// host.ts — wraps wit-bindgen generated bindings
export namespace log {
  export function info(msg: string): void;
  export function warn(msg: string): void;
  export function error(msg: string): void;
}

export namespace kv {
  export function get(key: string): Uint8Array | undefined;
  export function set(key: string, value: Uint8Array): void;
}

export namespace events {
  export function publish(topic: string, payload: string): void;
  export function subscribe(pattern: string): bigint;
  export function unsubscribe(id: bigint): void;
}
```

### D4. 构建流程

```bash
# 1. TypeScript → JavaScript
npx tsc

# 2. JavaScript → WASM Component
npx jco componentize dist/plugin.js --wit ../../wit --world tool -o plugin.wasm
```

### D 文件清单

| 操作 | 文件 |
|------|------|
| 新增 | `pdk/javascript/packages/prx-pdk/package.json` |
| 新增 | `pdk/javascript/packages/prx-pdk/src/*.ts` |
| 新增 | `pdk/javascript/templates/tool/*` |
| 新增 | `pdk/javascript/examples/markdown-tool/*` |
| 新增 | `pdk/javascript/examples/rate-limiter-middleware/*` |
| 新增 | `pdk/javascript/README.md` |

### D 风险点

| 风险 | 影响 | 缓解 |
|------|------|------|
| jco/componentize-js 版本迭代 | API 变化 | 锁定版本 |
| JS WASM 二进制大 (~5-10MB) | 加载慢 | 文档说明 |
| 有限的 JS API 在 WASM 沙箱内 | 功能受限 | 明确支持/不支持列表 |

---

## E. Go PDK

**预估工时：** 4 天  
**依赖：** A1，B  
**可并行：** 与 C、D 并行

### E1. 版本要求

- **TinyGo:** ≥ 0.34（WASI Preview 2 支持）
- **Go:** ≥ 1.22
- **wit-bindgen-go:** 最新版本（仍在 active development）

### E2. Go Module 结构

**新增目录：** `pdk/go/`

```
pdk/go/
├── prx-pdk/
│   ├── go.mod               # module github.com/prx/prx-pdk-go
│   ├── go.sum
│   ├── pdk.go               # re-exports, convenience functions
│   ├── types.go             # ToolSpec, PluginResult, etc.
│   ├── host/
│   │   ├── log.go           # log.Info(), log.Warn()
│   │   ├── config.go        # config.Get(), config.GetAll()
│   │   ├── kv.go            # kv.Get(), kv.Set()
│   │   ├── events.go        # events.Publish(), events.Subscribe()
│   │   └── http.go          # http.Request()
│   └── gen/                  # wit-bindgen-go 生成的绑定
├── templates/
│   └── tool/
│       ├── main.go.tmpl
│       ├── go.mod.tmpl
│       ├── plugin.toml.tmpl
│       └── build.sh.tmpl
├── examples/
│   ├── hash-tool/
│   │   ├── main.go
│   │   ├── go.mod
│   │   ├── plugin.toml
│   │   └── build.sh
│   └── event-forwarder-hook/
│       ├── main.go
│       ├── go.mod
│       ├── plugin.toml
│       └── build.sh
└── README.md
```

### E3. 用户代码示例

```go
package main

import (
    "github.com/prx/prx-pdk-go/host"
    "github.com/prx/prx-pdk-go/types"
)

//go:generate wit-bindgen-go generate --world tool ../../wit

func GetSpec() types.ToolSpec {
    return types.ToolSpec{
        Name:        "hash_tool",
        Description: "Compute hash of input text",
        ParamsSchema: `{"type":"object","properties":{"input":{"type":"string"},"algo":{"type":"string","enum":["sha256","md5"]}}}`,
    }
}

func Execute(argsJSON string) types.PluginResult {
    host.Log.Info("Computing hash...")
    // ... implementation
    return types.PluginResult{Success: true, Output: hash}
}

func main() {} // Required by TinyGo
```

### E4. 构建流程

```bash
# TinyGo 编译为 WASM
tinygo build -target=wasip2 -o plugin.wasm .

# 或使用 wasm-tools 适配
wasm-tools component new plugin.wasm -o plugin.component.wasm
```

### E 风险点

| 风险 | 影响 | 缓解 |
|------|------|------|
| wit-bindgen-go 不成熟 | 可能有 bug | 手写绑定 fallback；跟踪上游 issue |
| TinyGo 不支持所有 Go 标准库 | 功能受限 | 文档说明限制；提供兼容性列表 |
| Go WASM 二进制较大 (~2-5MB) | 可接受 | TinyGo 优化；strip |
| WASI Preview 2 在 TinyGo 中是 newer feature | 不稳定 | 锁定 TinyGo 版本 |

---

## F. prx-plugin CLI 工具

### F1. CLI 基础框架（与 A 并行）

**预估工时：** 2 天  
**依赖：** 无

**新增目录：** `pdk/cli/`

```
pdk/cli/
├── Cargo.toml
├── src/
│   ├── main.rs
│   ├── commands/
│   │   ├── mod.rs
│   │   ├── new.rs       # prx-plugin new
│   │   ├── build.rs     # prx-plugin build
│   │   ├── validate.rs  # prx-plugin validate
│   │   ├── test.rs      # prx-plugin test
│   │   └── pack.rs      # prx-plugin pack
│   ├── detect.rs        # 语言检测
│   └── mock_host.rs     # 本地测试用 mock host
└── README.md
```

### F1.1 命令设计

```bash
# 创建新插件项目
prx-plugin new <name> --lang <rust|python|js|go> --capability <tool|hook|middleware|cron>
# 例: prx-plugin new my-weather-tool --lang rust --capability tool

# 构建
prx-plugin build [--release] [--lang <lang>]
# 自动检测语言（Cargo.toml → Rust, package.json → JS, pyproject.toml → Python, go.mod → Go）

# 验证
prx-plugin validate [plugin.wasm]
# 检查: WIT 兼容性、manifest 完整性、权限声明合理性、export 函数签名

# 本地测试
prx-plugin test [--mock-host]
# 启动 mock host，加载插件，执行预定义测试用例

# 打包
prx-plugin pack [--output <path>]
# 生成 .prxplugin 文件（tar.gz: plugin.wasm + plugin.toml + README）
```

### F1.2 语言检测机制

```rust
fn detect_language(dir: &Path) -> Option<Language> {
    if dir.join("Cargo.toml").exists() { return Some(Language::Rust); }
    if dir.join("go.mod").exists() { return Some(Language::Go); }
    if dir.join("package.json").exists() { return Some(Language::JavaScript); }
    if dir.join("pyproject.toml").exists() || dir.join("setup.py").exists() {
        return Some(Language::Python);
    }
    None
}
```

### F1.3 Mock Host

用于 `prx-plugin test` 的本地测试环境：

```rust
struct MockHost {
    kv: HashMap<String, Vec<u8>>,
    config: HashMap<String, String>,
    events: Vec<(String, String)>,  // 记录发布的事件
    log_messages: Vec<(String, String)>,  // 记录的日志
}
```

Mock Host 实现所有 host interface，记录调用但不连接真实后端。

### F1.4 打包格式 (.prxplugin)

```
my-tool.prxplugin  (tar.gz)
├── plugin.toml        # manifest
├── plugin.wasm        # compiled component
├── README.md          # optional
├── LICENSE            # optional
└── checksums.sha256   # integrity verification
```

### F2. CLI 完善

**预估工时：** 1 天  
**依赖：** B/C/D/E 至少一个 PDK 完成

- 各语言构建命令的具体实现
- 错误提示和安装引导
- `--verbose` 日志
- `prx-plugin init` (在现有目录初始化)

### F 文件清单

| 操作 | 文件 |
|------|------|
| 新增 | `pdk/cli/Cargo.toml` |
| 新增 | `pdk/cli/src/main.rs` |
| 新增 | `pdk/cli/src/commands/*.rs` |
| 新增 | `pdk/cli/src/detect.rs` |
| 新增 | `pdk/cli/src/mock_host.rs` |

### F 风险点

| 风险 | 影响 | 缓解 |
|------|------|------|
| 各语言工具链安装复杂 | 用户体验差 | CLI 自动检测并给出安装指令 |
| mock host 与真实 host 行为差异 | 测试不可靠 | 共享 WIT 定义，mock 基于同一接口 |
| .prxplugin 格式后续可能变化 | 向后兼容 | 包含 format_version 字段 |

---

## 集成测试计划

**预估工时：** 2 天  
**依赖：** A + B 完成

### 测试矩阵

| 测试 | 范围 | 命令 |
|------|------|------|
| 无 feature 编译 | 确认 feature gate 不破坏 | `cargo test` |
| wasm-plugins feature | 所有 plugin 测试 | `cargo test --features wasm-plugins` |
| EventBus 单元测试 | publish/subscribe/wildcard | `cargo test event_bus` |
| Rust PDK 示例 | base64-tool 加载运行 | `cd pdk/rust/examples/base64-tool && cargo component build && cp plugin.wasm ../../plugins/` |
| HookManager 桥接 | 生命周期事件到达 EventBus | 集成测试 |
| Plugin reload with events | 热加载不丢失订阅 | 集成测试 |

### 回归测试检查清单

- [ ] `cargo test` 通过（无 feature）
- [ ] `cargo test --features wasm-plugins` 通过
- [ ] `cargo clippy --features wasm-plugins` 无 warning
- [ ] 现有 base64 example manifest 仍可加载
- [ ] Gateway API `/api/plugins` 仍正常
- [ ] HookManager 原有 hooks.json 行为不变
- [ ] PluginRegistry 接口无 breaking change

---

## 工时汇总

| 任务 | 工时 | 依赖 | 可并行 |
|------|------|------|--------|
| A1. EventBus WIT + Host Functions | 2d | 无 | - |
| A2. EventBus 集成 + 测试 | 1d | A1 | - |
| B. Rust PDK | 3d | A1 | 与 F1 并行 |
| C. Python PDK | 5d | A1, B | 与 D, E 并行 |
| D. JavaScript PDK | 5d | A1, B | 与 C, E 并行 |
| E. Go PDK | 4d | A1, B | 与 C, D 并行 |
| F1. CLI 基础框架 | 2d | 无 | 与 A, B 并行 |
| F2. CLI 完善 | 1d | B/C/D/E | - |
| 集成测试 + 文档 | 2d | All | - |
| **关键路径总计** | **~11d** | A→B→集成 | |
| **总人天** | **~23d** | | ~4.5 周 |

**关键路径：** A1(2d) → A2(1d) → B(3d) → 集成测试(2d) = 8d（加上并行的 C/D/E 中最长的 5d ≈ 11d 关键路径）

---

## 扩展性考量

### 未来插件间 RPC

EventBus 的 fire-and-forget 设计预留了 RPC 扩展路径：
- 发布 `rpc.request.{target_plugin}.{method}` 事件（payload 含 request_id）
- 目标插件处理后发布 `rpc.response.{request_id}` 事件
- 调用方通过订阅 response topic 获取结果
- 未来可在 host 层封装 `call()` 函数做语法糖

### 未来新增 Capability World

PDK 设计基于 WIT world 系统，新增 capability 只需：
1. 新增 `wit/plugin/<capability>.wit`
2. 在 `wit/worlds.wit` 新增 world
3. 各 PDK 添加对应模板
4. CLI 添加 `--capability <new>` 选项

### 未来新增语言

CLI 的语言检测和构建命令是插件化设计：
```rust
enum Language { Rust, Python, JavaScript, Go, /* future: */ Swift, Zig, CSharp }
```
新增语言只需：
1. 添加 Language variant
2. 实现 `detect()` 和 `build()` trait
3. 添加模板目录

### 未来插件市场/仓库

.prxplugin 打包格式支持：
- 版本化（manifest 中的 version）
- 校验（checksums.sha256）
- 元数据完整（author, license, description）
- 未来可建立 registry 索引（类似 crates.io / npm）

---

## 总结

P4 的核心交付物：
1. **EventBus** — 进程内异步事件总线，支持通配符订阅，64KB payload 限制，审计日志
2. **Rust PDK** — prx-pdk crate + proc-macro + 2 个示例插件
3. **Python PDK** — prx_pdk pip 包 + 装饰器 + 2 个示例
4. **JS/TS PDK** — @prx/pdk npm 包 + TypeScript 类型 + 2 个示例
5. **Go PDK** — prx-pdk Go module + 2 个示例
6. **prx-plugin CLI** — new/build/validate/test/pack 五个核心命令

全部不破坏 P1-P3 的 feature gate 隔离和现有接口。
