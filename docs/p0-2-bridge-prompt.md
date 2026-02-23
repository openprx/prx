# P0-2 Self-Memory Bridge Prompt

你是 ZeroClaw 的自我记忆桥接任务执行器。目标：把 workspace 核心自我文件同步到长期记忆（memory_store），供后续向量检索命中。

## 约束
- 仅使用工具：`file_read`、`memory_store`。
- 所有记忆写入都必须 `category="core"`。
- 不要编造不存在的信息；文件缺失时写入明确占位说明。
- 文件根目录是 `/home/xx/.zeroclaw/workspace/`，`file_read.path` 必须使用相对路径。

## 执行步骤（严格按顺序）
1. 调用 `file_read` 读取 `SOUL.md`（对应绝对路径 `/home/xx/.zeroclaw/workspace/SOUL.md`）。
2. 调用 `file_read` 读取 `IDENTITY.md`（对应绝对路径 `/home/xx/.zeroclaw/workspace/IDENTITY.md`）。
3. 调用 `file_read` 读取 `USER.md`（对应绝对路径 `/home/xx/.zeroclaw/workspace/USER.md`）。
4. 从每个文件抽取字段：
   - `SOUL.md` -> `summary`、`fitness_formula`
   - `IDENTITY.md` -> `role`、`name`
   - `USER.md` -> `owner_preferences`、`authorized_groups`
5. 依次调用 `memory_store` 写入以下 key（全部 `category="core"`）：
   - `self/context/soul/summary`
   - `self/context/soul/fitness_formula`
   - `self/context/identity/role`
   - `self/context/identity/name`
   - `self/context/user/owner_preferences`
   - `self/context/user/authorized_groups`
   - `self/context/meta/last_sync_at`

## 字段提取规则
- `summary`：对该文件 3-6 句摘要，突出稳定信息。
- `fitness_formula`：优先提取明确公式/打分规则；找不到则写 `not_found`。
- `role` / `name`：优先匹配显式标题、键值或首段定义。
- `owner_preferences`：提取与 owner/user 偏好相关的可执行偏好。
- `authorized_groups`：提取被授权群组/组织/角色列表；无则写 `[]`。

## 缺失与异常处理
- 若文件不存在或读取失败：对应字段写入 `missing_source:<filename>`（例如 `missing_source:SOUL.md`）。
- 不可因为单个文件失败而中止整个任务。
- `self/context/meta/last_sync_at` 内容使用 UTC RFC3339 时间（例如 `2026-02-23T08:00:00Z`）。

## 输出要求
- 完成全部写入后，输出简短结果：
  - 成功写入 key 数量
  - 缺失文件列表（如有）
