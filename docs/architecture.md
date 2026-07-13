# MiniCore 架构

本文档是 MiniCore 原生 Agent harness runtime core 的总入口。MiniCore 本仓库提供可嵌入的运行时核心、协议、会话、资源、工具、事件和 harness 编排能力；CLI、TUI 和 GUI 产品会在独立仓库中以 MiniCore 为核心接入。详细能力已经按可实现的编程模块拆到 `docs/modules/`，避免把所有设计挤在一个长文档里。

MiniCore 以明确 owner 和窄 seam 组织产品级 Agent 编排：`AgentRuntime` 是 UI 无关门面，`SessionManager` 管理会话生命周期，`SessionRuntime` 编排单个会话，`Driver` 适配底层 Agent SDK。pi、Codex 等项目可以作为设计参考，但除非文档明确标注兼容契约，否则其类型、调用方式和行为都不是 MiniCore 的兼容目标。

## 设计定位

MiniCore 使用 Rig 作为原生 Agent SDK，但 Rig 必须保持为实现细节。下游 CLI、TUI 和 GUI 宿主只通过运行时 command、query、event 和 snapshot 交互，不依赖 Rig 类型、模型提供方类型或工具实现细节。

MiniCore 不重新实现 Rig 的核心 Agent loop。Rig 负责 `AgentRun` / `AgentRunStep` 的状态机推进；MiniCore 通过 `Driver` 适配 Rig，在 `CallTools` 时经 `DriverHost::invoke_tool_batch(...)` 回到 `SessionRuntime`，再由 session-scoped `Tools` 子系统执行工具治理，并把底层活动映射成产品事件。

MiniCore 也不重新实现 provider HTTP clients。真实模型调用通过 `ModelGateway` 复用 Rig provider system；MiniCore 在该边界内治理 provider/model 解析、凭据、custom base URL、fallback、usage 和错误分类；后期 provider hook 也归该边界所有。`ModelGateway` 的最小稳定 spine 必须早于真实 `Driver` 集成，避免阶段 5 写临时 provider/auth 路径。

## 分层

```text
Downstream CLI     Downstream TUI     Downstream GUI
      │                  │                  │
      ▼                  ▼                  ▼
CLI Adapter       Ratatui Adapter     Tauri/Vue Adapter
      │                  │                  │
      └────── AgentRuntimeProtocol ─────────┘
                         │
                         ▼
                 MiniCore AgentRuntime
                  ┌──────┴──────┐
                  ▼             ▼
               EventBus  WorkspaceServices
       ┌──────────────┬──────────────┬──────────────┬──────────────┐
       ▼              ▼              ▼              ▼              ▼
 SessionManager  ResourceManager  CommandManager Future RuntimeHooks  ModelGateway
       │              │                                           │
       │              ├── ResourceSnapshotStore                   └──▶ Rig providers
       │              │     ├── current runtime -> RuntimeResourceSnapshot
       │              │     └── (workspace_id, cwd) -> CwdResourceSnapshot
       │              └── ResourceOverlayPolicy
       │
 LoadedSessionRuntimes
       │
 SessionRuntimeHandle ──▶ SessionRuntime actor ──▶ run-scoped RunTask ──▶ Driver ──▶ Rig AgentRun segment(s)
                               │
                               ├─ fixed workspace cwd; run captures TurnResourceSnapshot into TurnState
                               ├─ Command ───────▶ CommandManager materialize / parse / resolve
                               ├─ Tools ─────────▶ tool registry / policy / approval / executors
                               └─ PromptTurn ────▶ captured PromptResourceView + atomic PromptCallProfile

 Driver ──▶ prompt::project_model_call(PromptCallProfile + call-time lanes)
        └─▶ ModelInputProjection before each model call
```

## 文档地图

- [模块总览](modules/README.md)：整体模块关系、Rig / Runtime / 下游 UI 宿主的边界。
- [AgentRuntime](modules/agent-runtime.md)：UI 无关的运行时门面、`WorkspaceServices` / `ResourceSnapshotStore`、会话打开/聚焦和工作区生命周期。
- [SessionRuntime](modules/session-runtime.md)：单会话产品级编排、阶段、`PromptDelivery` admission、消息队列、`CommandRunPolicy::QueueAfterRun` 产生的结构化 pending session actions、turn state 和 post-run 流程；后期在其拥有的安全点接入 Hook。
- [AgentRuntimeProtocol](modules/agent-runtime-protocol.md)：`agent_runtime_protocol::AgentCommand`、`agent_runtime_protocol::Event`、`agent_runtime_protocol::EventMsg`、`agent_runtime_protocol::RuntimeSnapshot` 和下游 adapter 调用方式。
- [AgentRuntimeEvents](modules/agent-runtime-events.md)：`agent_runtime_protocol::Event { ..., msg }`、事件生命周期、commit 后领域事实、重连和跨模块事件顺序。
- [SessionManager / SessionWriter / SessionStorage](modules/session-manager.md)：会话生命周期、已加载会话运行时、统一 stable batch writer、追加式 session tree、一行一 batch 的 JSONL adapter 和上下文重建。
- [ResourceManager](modules/resource-manager.md)：资源来源聚合、刷新和资源诊断。
- [CommandSurface](modules/command-surface.md)：跨 UI 的用户命令领域面、无状态 `CommandManager`、session-scoped `Command`、nested JSON command tree、dynamic providers、handler registry 和执行前 resolve。
- [RuntimeHooks](modules/runtime-hooks.md)：后期内部 hook seam、hook/event 边界、capability、typed result、owner 分层和安全点。
- [Skills](modules/skills.md)：`skills.rs` 平级模块，提供技能 metadata、catalog、发现、解析、校验和格式化 helper。
- [PromptTemplates](modules/prompt-templates.md)：`prompt_templates.rs` 平级模块，定义模板资源、参数语法和单次展开 helper。
- [Prompt](modules/prompt.md)：`prompt.rs` / `prompt/` 无状态提示词组装子系统，定义 immutable `PromptTurn`、原子 `PromptCallProfile` 和最终 `ModelInputProjection`。
- [Driver](modules/driver.md)：Rig `AgentRun` / `CallModel` / `CallTools` 的适配职责。
- [ModelGateway](modules/model-gateway.md)：provider/model/auth 执行边界、custom provider、Rig provider adapter、usage/error/fallback 规则。
- [Tools](modules/tools.md)：`tools.rs` / `tools/` session-scoped 工具子系统，封装工具定义、registry、active tools、policy、approval、grants、execution coordination、sandbox、mutation locks 和 executors。
- [Compaction](modules/compaction.md)：`compaction.rs` 平级模块，提供上下文压缩准备、摘要 prompt、压缩摘要消息和自动压缩语义。
- [UsageStats](modules/usage-stats.md)：token 消耗、run/session stats、context usage 和 UI 展示口径。

## 核心边界

- 下游 CLI/TUI/GUI 不能导入 Rig 类型。
- 下游 CLI/TUI/GUI 不能直接调用模型提供方、执行工具、读取凭据、扫描技能或读写会话文件。
- Rig 拥有 agent loop 的协议级状态机，不拥有产品级工具治理、会话持久化或 UI 呈现。
- `AgentRuntime` 是下游 CLI/TUI/GUI 共用的稳定 runtime 门面；其 interface 分为 `dispatch`、`query`、`subscribe` 和 `snapshot`，mutation/异步工作、只读数据、运行变化和恢复读模型不能混用通道。
- `SessionManager` 协调持久化会话和已加载会话运行时；`LoadedSessionRuntimes` 保存显式 `SessionRuntimeHandle`，由 factory 为每个 loaded session 启动独立 actor。所有 session mutation 通过 `SessionWriter.commit(SessionWriteBatch)`，只提交协议完整的稳定单元；多个 `SessionRuntime` actor 可以同时推进 work，每个 runtime 固定自己的 workspace cwd，并在每次 run 捕获该 cwd 当前 `TurnResourceSnapshot` 进 `TurnState`。
- `ResourceManager` 维护级联资源快照：`RuntimeResourceSnapshot` 被 `CwdResourceSnapshot` pin 住，`CwdResourceSnapshot` 被 `TurnResourceSnapshot` pin 住，MVP 只预留 `StepResourceSnapshot` 类型。cwd snapshot 通过内置 `ResourceOverlayPolicy` 把 cwd/project 资源覆盖到 runtime/global 资源之上，产出该 cwd 下的 resolved view。
- `SessionRuntime` 是单个会话的产品级编排层、per-session actor 和 Prompt Pull Master。它持续处理 command 与 run control effect，拥有 phase、queues、`CurrentRun` projection、commit 和公共事件归约；每次公开启动的 run 由短期 `RunTask` 持有 `Driver`，Driver 通常推进一个 Rig `AgentRun`，active Steer 时可在同一 `RunId` 下顺序 rollover 多个 Rig segment。Driver 只接收包含原子 `PromptCallProfile` 的窄 `DriverTurnInput`，并通过私有 `RunLink` 回到 owner actor。
- `CommandSurface` 属于运行时用户命令入口：下游 UI 可以渲染 autocomplete / command palette / 嵌套菜单 / picker，但不拥有 command text 的权威解析、catalog selection 的授权、执行映射或用户可见结果语义。`CommandManager` 无状态共享；每个 `SessionRuntime` 通过 session-scoped `Command` 提供当前 session 的 command view。
- `RuntimeHooks` 是 MiniCore 后期内部扩展点系统。当前 MVP 不实现 hook registry / hook invocation；设计上先固定 owner 分层。Hook 后续可以在安全点返回 typed decision / patch / replacement，但不能直接发布 `agent_runtime_protocol::Event`、读写 session storage、执行工具或读取凭据。
- `Driver` 是 Rig 状态机和产品运行时之间的执行适配层。
- `ModelGateway` 是真实模型调用边界；`Driver` 只传 `ModelSelection`，不解析 provider、凭据、base URL 或 raw payload。
- 工具注册、活跃工具、审批、授权记忆、路径授权、sandbox enforcement capability、mutation lock 和真实副作用执行由 `SessionRuntime` 持有的 session-scoped `Tools` 子系统统一治理；active `RunTask` 通过 `DriverHost::invoke_tool_batch(...) -> ToolBatchInvoker.invoke_batch(...)` 进入工具管线，stable commit、审批 control 和公共事件仍回到 owner actor。MVP 不启用通用 `bash`；请求的子进程限制无法由 OS-native/external backend 强制时必须 fail closed。
- 上下文压缩由 `SessionRuntime` 编排，`Compaction` 模块提供准备、摘要 prompt 和压缩摘要消息 helper；`Driver` 不执行压缩。
- 技能、提示模板、上下文文件、会话管理与会话存储属于 MiniCore 运行时，不属于下游 UI。资源身份、overlay 和 snapshot 归 `ResourceManager`；结构化 intent 展开和最终模型输入组装归 Prompt。

## 相关决策

- [ADR 0001：在 UI 无关的 Agent 运行时后使用 Rig](adr/0001-use-rig-behind-agent-runtime.md)
- [ADR 0002：上下文压缩由 SessionRuntime 编排](adr/0002-compaction-is-session-runtime-owned.md)
- [ADR 0003：AgentRuntimeEvents 使用 EventMsg、配对生命周期和单 run 终态](adr/0003-agent-runtime-events-use-event-msg-and-lifecycle-pairs.md)
- [ADR 0004：SessionManager 拥有已加载会话运行时](adr/0004-session-manager-owns-loaded-session-runtimes.md)
- [ADR 0005：ResourceManager 是运行时内部资源服务](adr/0005-resource-manager-is-runtime-internal.md)
- [ADR 0006：CommandSurface 是跨 UI 的运行时命令入口](adr/0006-command-surface-is-runtime-command-surface.md)
- [ADR 0007：CommandSurface 使用 UI-safe 命令结果表达用户可见反馈](adr/0007-command-surface-uses-command-presentation.md)
- [ADR 0012：命令体系使用无状态 CommandManager 和 session-scoped Command](adr/0012-command-manager-is-stateless-session-command-facade.md)
- [ADR 0008：RuntimeHooks 是内部安全点扩展缝，不是协议事件或 UI 插件 API](adr/0008-runtime-hooks-are-internal-safe-point-seams.md)
- [ADR 0009：ModelGateway 包装 Rig providers](adr/0009-model-gateway-wraps-rig-providers.md)
- [ADR 0010：多 session runtime 使用级联资源快照](adr/0010-use-per-cwd-resource-snapshots-for-multi-session-runtime.md)
- [ADR 0011：Tools 是 SessionRuntime 内部的 Session-Scoped 子系统](adr/0011-tools-are-session-scoped-subsystem.md)
- [ADR 0013：Driver 接收 DriverTurnInput 而不是完整 TurnState](adr/0013-driver-receives-driver-turn-input.md)
- [ADR 0014：ModelGateway spine 先于真实 Driver 集成](adr/0014-model-gateway-spine-precedes-driver-integration.md)
- [ADR 0015：Hook owner 遵循 runtime 边界](adr/0015-hook-owners-follow-runtime-boundaries.md)
- [ADR 0016：命令运行策略与提示词交付方式分离](adr/0016-separate-command-run-policy-from-prompt-delivery.md)
- [ADR 0017：Prompt 使用不可变 turn 组装而不是长期 Manager](adr/0017-prompt-uses-immutable-turn-assembly.md)
- [ADR 0018：AgentRuntime 分离 Command、Query、Event 和 Snapshot](adr/0018-agent-runtime-separates-command-query-event-and-snapshot.md)
- [ADR 0019：会话写入使用统一可信的 batch writer](adr/0019-session-writes-use-one-trusted-batch-writer.md)
- [ADR 0020：AgentRuntime 不拥有当前会话](adr/0020-agent-runtime-has-no-current-session.md)
- [ADR 0021：SessionRuntime 分离 actor 控制面与 run 执行](adr/0021-session-runtime-separates-actor-control-from-run-execution.md)
- [ADR 0022：Workspace 是单实例薄边界容器](adr/0022-workspace-is-single-instance-thin-boundary.md)
