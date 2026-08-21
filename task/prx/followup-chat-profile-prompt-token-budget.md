# Followup（低优先）：chat 画像 prompt 注入 token 预算口径

来源：Batch F 审计 NIT（2026-07-06，commit `3c0095f7`）。

## 现状
派工单 item 9 要求 "## Current Conversation" 注入块 **总预算 ≤150 token**。
实际实现是**按字符分字段截断**（channels/mod.rs `prompt_trim`）：purpose 180 / notes 240 / title 80 / you 48 / reply_target 96 / tags 120 chars。极端全满时整块约 **190 token**，可能略超 150。

## 判定
非安全/正确性问题，块大小有界，仅口径差异。审计 ACCEPT，不阻塞会话画像战役收口。

## 若要收紧（可选）
- 方案 A：把各字段字符上限整体下调，使满载 ≤150 token（例如 purpose 120 / notes 160）。
- 方案 B：改为对渲染后整块做一次 150-token 上限的尾部截断（需引 tokenizer 或用 chars≈token 近似）。
- 建议 A（简单、无新依赖、字段级可控）。

优先级：低。等用户对画像 UX 有实际反馈再定是否动。
