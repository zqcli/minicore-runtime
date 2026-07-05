# SessionManager owns loaded session runtimes

我们将独立的 `SessionRuntimeRegistry` 架构层合并进广义 `SessionManager`，并把 live runtime map 命名为 `LoadedSessionRuntimes`。这样 `SessionManager` 负责工作区内 session 生命周期：持久化会话目录、`SessionHandle` / `SessionStorage`、focused session 和已加载 `SessionRuntime` 的打开、查找、替换与关闭；`AgentRuntime` 保持为 UI-facing facade、事件通道和工作区服务 owner。

这个决定参考 Codex `ThreadManager` 同时协调 live threads 与 `ThreadStore`、但仍保留 `ThreadStore` / `LiveThread` / runtime session 分离的做法。`SessionManager` 可以管理 live runtime 生命周期，但不能执行 Agent run、调用 Rig、执行工具、计算 usage 或发布 UI 事件；这些仍属于 `SessionRuntime`、`Driver`、`ToolGateway`、`UsageStats` 和 `AgentRuntime`。
