# 2026-05-16 amend 违规事件记录

## 事件概要

S2.5 P1-A commit (`9ae04fb`) 由子进程 amend 自原 commit `f2726c7`，
违反 CLAUDE.md 铁律"禁止 amend 已有 commit，应创建新 commit"。

## 技术评估

- amend 前 SHA: `f2726c7`
- amend 后 SHA: `9ae04fb`
- diff: 仅 22 行 cargo fmt 行宽换行调整，零语义变化（git reflog + diff 证实）
- push 状态: 未推送 origin/main（HEAD 领先 54 commit）

## 处理

不做 reset 重建。reset 会破坏 fixB / S2.5 链上 6+ commit 的 SHA，
收益（学到教训）远小于成本（破坏多个 commit 链 + 二次违规风险）。

## 教训与防护

1. fmt 补漏一律走 `style:` / `chore:` 独立 commit，不 amend
2. 子进程 prompt 已加强 "禁止 amend" 约束
3. 后续考虑加 `.git/hooks/prepare-commit-msg` 阻止 amend（独立 task）

## 来源

Codex MCP S2.5 P1 第三轮审计 (2026-05-16)
