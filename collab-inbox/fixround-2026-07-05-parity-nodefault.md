# [派工] no-default-features 编译回归（ISS-039 同款门，push 阻塞级）

全量自检发现：`cargo check --workspace --no-default-features` FAIL（exit 101，E0433 ×3）。引入 commit `508e93e9`（F2 slash menu parity）。

## 问题

`SlashProviderModelCatalog` 定义在 `chat::tui`（整个模块被 `#[cfg(feature = "terminal-tui")]` 门控），但三处非门控代码直接引用：
- `src/chat/action.rs:258` — `provider_model_catalog: Vec<crate::chat::tui::SlashProviderModelCatalog>`
- `src/chat/state.rs:309` — 同上（pub 字段）
- `src/chat/state.rs:2242` — `_provider_model_catalog: Vec<...>`
（state.rs:583/2219/2830 也有引用但处于门控区块内未报错，改动时一并核对。）

## 修法（推荐前者）

1. **类型迁出**：`SlashProviderModelCatalog`（及其紧耦合的纯数据类型）挪到无门控模块（如 `chat::state` 或单独 `chat::slash_types`），`chat::tui` 里 `pub use` re-export 保持既有引用不变。
2. 或对相关字段/Action variant 加同款 `#[cfg(feature = "terminal-tui")]`（会传染到构造点，较脏）。

## 验收

- `cargo check --workspace --no-default-features` 通过（**必须实际跑**）
- `cargo check --workspace --all-features` + `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets` + `cargo test --bin prx` 不回归
- 一个 commit；干净状态自检；receipt 写 `collab-outbox/receipt-2026-07-05-parity-nodefault.md`。禁止 push。
