# SessionManager 拥有已加载会话运行时

> **归档（V1）**：本 ADR 属于 MiniCore V1 架构，仅作历史参考，不得作为当前实现或新开发的设计依据。当前权威决策见 `docs/adr/`（0100+）。原文保持历史原貌。

状态：service generation 注入部分已由 [ADR 0010](0010-use-per-cwd-resource-snapshots-for-multi-session-runtime.md) 取代。

我们将独立的 `SessionRuntimeRegistry` 架构层合并进广义 `SessionManager`，并把 live runtime map 命名为 `LoadedSessionRuntimes`。这样 `SessionManager` 负责工作区内 session 生命周期：持久化会话目录、`SessionHandle` / `SessionStorage`，以及已加载 `SessionRuntime` 的打开、查找与关闭；`AgentRuntime` 保持为 adapter-facing facade、事件通道和 `WorkspaceServices` owner。core 不保存 focused/current session，所有 session-scoped command 显式携带 `SessionId`，客户端选择属于 adapter-local state。多个 `SessionRuntime` 可以同时 loaded 并推进 work；每个 runtime 固定自己的 workspace cwd，后续 user turn 通过共享 `ResourceManager` 捕获 `TurnResourceSnapshot`，不能因客户端 selection 变化而改写。删除 runtime-global current session 与 all-loaded snapshot 的后续决策见 [ADR 0020](0020-agent-runtime-has-no-current-session.md)。

这个决定参考 Codex `ThreadManager` 同时协调 live threads 与 `ThreadStore`、但仍保留 `ThreadStore` / `LiveThread` / runtime session 分离的做法。`SessionManager` 可以管理 live runtime 生命周期，但不能执行 Agent run、调用 Rig、执行工具、计算 usage 或发布 UI 事件；这些仍属于 `SessionRuntime`、`Driver`、`Tools`、`UsageStats` 和 `AgentRuntime`。
