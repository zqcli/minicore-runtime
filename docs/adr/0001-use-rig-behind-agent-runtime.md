# 在 UI 无关的 Agent 运行时后使用 Rig

状态：资源作用域部分已由 [ADR 0010](0010-use-per-cwd-resource-snapshots-for-multi-session-runtime.md) 细化；工具子系统边界已由 [ADR 0011](0011-tools-are-session-scoped-subsystem.md) 细化。

我们将围绕 Rig 构建 MiniCore 原生 Agent harness runtime core，但会把 Rig 放在 UI 无关的 `AgentRuntime` 之后。该运行时向下游 CLI、TUI 和 GUI 适配器暴露运行时命令、运行时事件和快照。我们不会在 WebView 中运行 pi coding-agent SDK，因为 pi SDK 面向 Node 运行时，并依赖文件系统、进程、凭据和资源加载能力；这些能力应该属于可信宿主。我们也不会重新实现 Rig 的核心 Agent loop；运行时会提供 `Driver`，用来推进 Rig 的 `AgentRun`、通过 `DriverHost` 把工具调用委托给 `SessionRuntime` 内部的 `Tools` 子系统、产出可持久化运行结果，并把底层活动映射成产品事件。

我们不会复制 pi-agent-core 的 `AgentHarness` 作为一个独立架构层。pi coding-agent 的生产路径更接近 `AgentSessionRuntime -> AgentSession -> Agent -> agent-loop`：`AgentSessionRuntime` 负责会话替换和 cwd 绑定服务，`AgentSession` 承担产品级编排能力，底层 `Agent` 和 `agent-loop` 负责运行。这个经验说明，本项目也应该把产品级编排能力放进单个会话的 `SessionRuntime`，而不是暴露或实现一个独立的通用编排层。

`AgentRuntime` 是下游 CLI、TUI 和 GUI 共用的稳定门面，负责命令、事件、快照、工作区、会话目录和 `WorkspaceServices`。`WorkspaceServices` 中的共享 `ResourceManager` 管理级联资源快照；每个 `SessionRuntime` 固定 workspace cwd，并在 user turn 启动时捕获 `TurnResourceSnapshot`。`SessionRuntime` 是单个会话的产品级编排层，负责阶段控制、turn state、pending session writes、steering/follow-up/next-turn 队列、prompt/skill/prompt-template 调用、模型与思考等级切换、工具与活跃工具切换、资源更新、stream options、abort/waitForIdle、事件映射和内部运行时 Hook。Rig 负责核心 agent loop，`SessionRuntime` 和 `Driver` 负责把它产品化为可被下游宿主稳定调用和观察的后端能力。

工具能力同样遵循这个边界：Rig 负责工具调用的协议级状态机，例如模型何时请求工具、工具结果如何回填以及之后是否继续模型调用。我们不使用 Rig 高阶 runner 自动执行工具；主路径采用 Rig `AgentRun / AgentRunStep` 的 sans-IO 驱动方式。`Driver` 在 `CallTools` step 调用 `DriverHost::invoke_tool_batch(...)`，host 实现再由 `SessionRuntime` 转发给内部 session-scoped `Tools` 子系统，最后把结果喂回 `AgentRun::tool_results(...)`。`Tools` 封装工具定义、注册表、活跃工具、系统提示词工具说明、工具策略、审批、授权记忆、沙箱、执行协调和内置/外部 executor implementations；会话持久化和 UI 事件归约仍由 `SessionRuntime` 持有和调度。

技能加载、提示模板、上下文文件等模型资源也属于 Agent 运行时，而不是 UI。运行时会参考 pi coding-agent `DefaultResourceLoader` 的做法加载技能、维护诊断、生成模型可见技能摘要，并通过显式 `InvokeSkill` 命令把完整技能内容注入某次 Agent 运行。

会话加载与管理同样属于 Agent 运行时。运行时会参考 pi coding-agent `SessionManager`、`AgentSessionRuntime` 的会话替换经验，以及 Codex `ThreadManager` / `ThreadStore` / `LiveThread` 的分工；本项目采用广义 `SessionManager` 协调持久化会话目录和内部 `LoadedSessionRuntimes`，并用 `SessionHandle` / `SessionStorage` 保存追加式会话条目、当前叶子和从根到叶子的上下文重建。UI 只能通过运行时命令打开、列出、fork、导航或删除会话，不能直接读写会话文件，也不能直接取得 live `SessionRuntime`。

这个决策让 MiniCore 保持小体积和原生 core 形态，同时让下游应用复用从 pi 学到的经验：事件驱动 UI 更新、工具调用可观测性、队列、会话持久化与分支、工作区绑定工具、技能资源加载、内部运行时 Hook，以及对高风险操作的显式策略控制。
