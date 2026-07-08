# SessionManager 拥有已加载会话运行时

状态：service generation 注入部分已由 [ADR 0010](0010-use-per-cwd-resource-snapshots-for-multi-session-runtime.md) 取代。

我们将独立的 `SessionRuntimeRegistry` 架构层合并进广义 `SessionManager`，并把 live runtime map 命名为 `LoadedSessionRuntimes`。这样 `SessionManager` 负责工作区内 session 生命周期：持久化会话目录、`SessionHandle` / `SessionStorage`、focused session 和已加载 `SessionRuntime` 的打开、查找、聚焦与关闭；`AgentRuntime` 保持为 UI-facing facade、事件通道和 `WorkspaceServices` owner。多个 `SessionRuntime` 可以同时 loaded/running；每个 runtime 固定自己的 workspace cwd，后续 user turn 通过共享 `ResourceManager` 捕获 `TurnResourceSnapshot`，不能随 focus 切换改写。

这个决定参考 Codex `ThreadManager` 同时协调 live threads 与 `ThreadStore`、但仍保留 `ThreadStore` / `LiveThread` / runtime session 分离的做法。`SessionManager` 可以管理 live runtime 生命周期，但不能执行 Agent run、调用 Rig、执行工具、计算 usage 或发布 UI 事件；这些仍属于 `SessionRuntime`、`Driver`、`Tools`、`UsageStats` 和 `AgentRuntime`。
