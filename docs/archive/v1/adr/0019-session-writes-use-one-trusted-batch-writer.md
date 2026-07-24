# 会话写入使用统一可信的 batch writer

> **归档（V1）**：本 ADR 属于 MiniCore V1 架构，仅作历史参考，不得作为当前实现或新开发的设计依据。当前权威决策见 `docs/adr/`（0100+）。原文保持历史原貌。

Status: Superseded in part by [ADR 0024](0024-session-storage-uses-by-entry-jsonl.md) on 2026-07-16

本 ADR 保留为历史决策记录。ADR 0024 保留“one trusted writer / one durable truth / append-before-publication”原则，但取代本文的一行一 batch、`SessionWriteBatch`、`StoredSessionBatch`、atomic ToolRound 和 batch-result leaf 协议。

MiniCore 中已创建会话的所有领域 mutation 都通过 `SessionWriter.commit(SessionWriteBatch)` 完成。session header 只由 storage factory 在 `SessionHandle` 暴露前原子初始化，不是第二条运行时写入通道。成功返回表示整个 batch 已按 storage adapter 的进程崩溃恢复契约写入，失败表示该 batch 不得出现在恢复投影中；`SessionRuntime`、`Driver`、`Tools` 和 command handler 都不能绕过这个 seam 直接追加 entry。

持久化只接收协议完整、可以独立恢复的稳定单元：user input、完整 assistant tool-call 与其全部 tool results、最终 assistant message、compaction、独立 session mutation 和 tree mutation。tree mutation 只能移动到 committed append batch 的边界，不能把多-entry tool round 切成 partial history。streaming delta、partial assistant、pending approval、执行中的 tool round 和其他 `CurrentRun` 状态只保留在内存；abort、failure 或 host crash 后恢复最后一个成功提交的 batch，不补 synthetic tool result，也不恢复中断中的 run。按 [ADR 0021](0021-session-runtime-separates-actor-control-from-run-execution.md)，同一 session 的 owner actor 线性化 abort 与 commit：commit admission 前观察到 abort 可以丢弃 candidate；commit 一旦开始不接受 run cancellation；graceful abort/close/shutdown 等待其得到确定结果，强制退出则按 crash recovery 处理。

公共协议不再发布 `persistence_save_point`。需要持久化的领域事实在 `commit()` 成功、runtime-owned required projections 应用后才发布对应事件；后期备份、telemetry 和可重建 cache 可以消费内部 `AfterSessionCommit` observer。observer 失败不能回滚已提交事实或阻止领域事件。MVP 不引入 `SessionRevision`：`CommittedSessionBatch` 返回 entry ids 与 current leaf 即可。JSONL adapter 必须一行编码一个完整 batch，加载时只忽略因进程中断造成的末尾不完整行；该契约不承诺断电级 durability，也不承诺工具副作用与 session 文件的 exactly-once 原子性。

## Amendment 2026-07-14 (ADR 0023)

[ADR 0023](0023-driver-starts-from-one-committed-conversation-seed.md) 在不改变单可信 writer 原则的前提下，增加了运行时投影术语：`CommittedSessionBatch` 返回 writer 最终分配 identity 后的 committed entries 与 leaf，actor 据此确定性构造 `CommittedConversationDelta`，并在需要时重建 `CommittedConversationState` / `ConversationSeed`。这些投影只来源于已提交 batch；未提交 assistant draft、tool result draft 或 compaction draft 仍不能推进 Driver/Rig working transcript。实现命名固定为 `commit_pending_messages`、`execute_and_commit_tool_round` 和 `commit_final_assistant_message` 等 stable-batch 边界。
