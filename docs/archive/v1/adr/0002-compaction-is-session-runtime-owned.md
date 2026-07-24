# 上下文压缩由 SessionRuntime 编排

> **归档（V1）**：本 ADR 属于 MiniCore V1 架构，仅作历史参考，不得作为当前实现或新开发的设计依据。当前权威决策见 `docs/adr/`（0100+）。原文保持历史原貌。

Status: Superseded by [ADR 0027](0027-compaction-uses-strict-stable-suffix.md)

保留原则：Compaction由session execution编排，不由Driver或AgentLoop拥有。SessionRuntime、SessionWriteBatch和具体helper形状已由SessionExecutor、by-entry writer及ADR 0027替代。

上下文压缩是会话上下文投影能力，而不是 Rig `AgentRun` 的协议 step。我们决定由 `SessionRuntime` 编排压缩，由 `compaction.rs` 提供准备、摘要 prompt 和压缩摘要消息 helper，由 `SessionHandle.commit(SessionWriteBatch::compaction(...))` 提交 compaction entry 与 leaf update，并由 `SessionStorage` 支持 root-to-leaf context reconstruction 让摘要替换旧历史；`Driver` 只暴露 usage、消息和错误事实，不执行压缩或摘要模型调用。这样避免 driver 持有 session tree、持久化、Hook、retry 和 UI 事件，也避免在压缩后 resume 仍包含旧 history 的 serialized `AgentRun`。
