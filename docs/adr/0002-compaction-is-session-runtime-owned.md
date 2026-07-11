# 上下文压缩由 SessionRuntime 编排

上下文压缩是会话上下文投影能力，而不是 Rig `AgentRun` 的协议 step。我们决定由 `SessionRuntime` 编排压缩，由 `compaction.rs` 提供准备、摘要 prompt 和压缩摘要消息 helper，由 `SessionHandle.commit(SessionWriteBatch::compaction(...))` 提交 compaction entry 与 leaf update，并由 `SessionStorage` 支持 root-to-leaf context reconstruction 让摘要替换旧历史；`Driver` 只暴露 usage、消息和错误事实，不执行压缩或摘要模型调用。这样避免 driver 持有 session tree、持久化、Hook、retry 和 UI 事件，也避免在压缩后 resume 仍包含旧 history 的 serialized `AgentRun`。
