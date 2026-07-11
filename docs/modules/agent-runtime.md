# AgentRuntime

`AgentRuntime` 是 MiniCore 的 UI 无关运行时门面，供下游 CLI、TUI 和 GUI 宿主通过 `AgentRuntimeProtocol` 接入。它不执行单个 Agent turn 的细节，而是管理工作区、运行时服务、事件通道，并通过 `SessionManager` 协调会话生命周期。

## Interface

对下游 adapter 暴露的 interface 保持很小：

```rust
use crate::agent_runtime_protocol as protocol;

pub trait AgentRuntime {
    async fn dispatch(&self, command: protocol::AgentCommand) -> Result<protocol::CommandAck, RuntimeError>;
    async fn query(&self, query: protocol::RuntimeQuery) -> Result<protocol::QueryResponse, protocol::QueryError>;
    fn subscribe(&self) -> protocol::EventStream;
    async fn snapshot(&self) -> Result<protocol::RuntimeSnapshot, RuntimeError>;
}
```

下游 CLI/TUI/GUI 不应该直接调用更细的方法。会话打开、资源刷新、模型切换、工具审批等 mutation/异步工作通过 `AgentCommand` 表达；session list、settings、资源详情、command catalog、model catalog、usage 和 diagnostics 等只读数据通过按领域分组的 `RuntimeQuery` 表达。

## 核心职责

- 处理下游 CLI/TUI/GUI adapter 发来的 `agent_runtime_protocol::AgentCommand`。
- 路由 `RuntimeQuery` 到 `SessionManager`、settings、`ResourceManager`、`CommandManager`、model/usage/diagnostics owner，并直接返回 `QueryResponse`；query 不发布业务事件。
- 发布所有 `agent_runtime_protocol::Event`，维护单调递增的 event sequence，并为后加入的订阅者生成带 `last_event_sequence` 的 `agent_runtime_protocol::RuntimeSnapshot`。
- 管理工作区，并把会话列表、打开、创建、删除、fork、import 和已加载会话运行时交给 `SessionManager` 协调。
- 通过 `SessionManager` 取得或加载 `SessionRuntime`，再把 session-scoped 命令路由给对应 `SessionRuntime`。
- 管理 `WorkspaceServices`，其中包含 `ResourceManager`、user-global settings/provider/auth、共享 `ModelGateway`、事件通道、无状态 `CommandManager` 和运行时诊断。
- 通过 `ResourceManager` 维护级联资源快照：current `RuntimeResourceSnapshot`、每 `(workspace_id, cwd)` 的 current `CwdResourceSnapshot`、run 启动时捕获进 `TurnState` 的 `TurnResourceSnapshot`，以及 MVP 只预留的 `StepResourceSnapshot`。
- 持有共享、无状态 `CommandManager`，并把 `ExecuteCommandText` / `ExecuteCatalogCommand` 路由到目标 `SessionRuntime.command`。`CommandManager` 负责 materialize catalog、parse、suggest、resolve、`CommandRunPolicy` 校验和 handler registry；session-scoped `Command` 负责构造当前 session 的 `CommandContext` / `SessionCommandHost`。prompt-producing 结果再由目标 `SessionRuntime` 按 `PromptDelivery` 统一 admission。
- 后期持有 `RuntimeHookRegistry` 作为内部 runtime service；当前 MVP 不实现 hook registry / hook invocation。启用后只在明确 owner 的安全点调用 hook，并把 hook 结果交给拥有状态机的模块应用。
- 发布 command result events，把 `/status`、`/usage`、`/model`、`/help` 等命令的用户可见结果表达为 display-neutral 输出或交互请求；runtime 不定义具体 picker、popup、menu、form 或 widget 组件。
- 在 session open、focus、new、fork、import、close 前后执行受控的 open/load/focus/unload 流程；focus 切换不隐式关闭旧 `SessionRuntime`。
- 保证下游 UI/CLI 不直接接触 Rig、工具实现、凭据、技能文件、会话文件或内部 driver/tool/hook event。

`OpenWorkspace` 建立 workspace 绑定的运行时服务和会话目录，并调用 `ResourceManager.ensure_runtime_snapshot(ResourceInitReason::WorkspaceOpen)` 初始化 runtime 级资源快照；它不默认聚焦或恢复任何旧 session。刚打开窗口时 `RuntimeSnapshot.active_session_id = None`、`RuntimeSnapshot.active_session = None`；TUI 的 `/resume` handler 可以读取 session index 并生成 interaction，GUI sidebar 使用 `RuntimeQuery::Session(SessionQuery::List { ... })`。只有 `OpenSession` / `NewSession` 成功后，`AgentRuntime` 才通过 `SessionManager` 创建或加载 `SessionRuntime`，并在后续 `RuntimeSnapshot.active_session` 中投影该会话的初始 idle 状态。

MVP 的 `AgentRuntime` 嵌入在 CLI/TUI/GUI host 进程内，和 UI host 同生命周期，不作为独立 daemon/server 存活。`subscribe()` / `snapshot()` 的 reconnect 语义用于同一程序上下文内的 late subscribe、reducer/subscriber 重建和 sequence gap recovery；不支持 UI adapter 失败但 runtime 继续运行、随后由新 UI 连接并恢复所有后台 session 的模式。

## 运行时服务

`RuntimeServices` 是内部总称，不是一套会随 focused session 改变而整体替换的全局单例。MVP 支持一个 UI/runtime 进程内多个 `SessionRuntime` 同时 loaded/running，但不使用 per-cwd 服务容器。`WorkspaceServices` 持有共享运行时服务；`ResourceManager` 按 runtime/cwd/turn/step 分层维护不可变资源快照；session 固定自己的 workspace cwd；run 启动时捕获当前 cwd 的 `TurnResourceSnapshot` 并构建 `TurnState`。

```text
AgentRuntime
  └─ WorkspaceServices
      ├─ EventBus
      ├─ SessionManager / SessionIndex
      ├─ CommandManager
      ├─ RuntimeHookRegistry / future hook service
      ├─ RuntimeDiagnostics
      ├─ ResourceManager
      │   ├─ ResourceSnapshotStore
      │   │   ├─ current runtime -> Arc<RuntimeResourceSnapshot rev-r>
      │   │   ├─ key: (workspace_id, cwd) -> Arc<CwdResourceSnapshot rev-a -> rev-r>
      │   │   └─ key: (workspace_id, cwd) -> Arc<CwdResourceSnapshot rev-b -> rev-r>
      │   ├─ ResourceResolver / loaders
      │   └─ ResourceOverlayPolicy
      ├─ SettingsStore / user-global EffectiveSettings
      ├─ ProviderRegistry / user-global provider catalog
      ├─ AuthStore / user-global credentials boundary
      └─ ModelGateway / shared model invocation boundary

LoadedSessionRuntimes
  ├─ SessionRuntime A { workspace_id, cwd: repo-a, command: Command }
  │   └─ current run captures TurnResourceSnapshot -> CwdSnapshot(repo-a, rev-10)
  ├─ SessionRuntime B { workspace_id, cwd: repo-a, command: Command }
  │   └─ next run captures current CwdSnapshot(repo-a)
  └─ SessionRuntime C { workspace_id, cwd: repo-b, command: Command }
      └─ current run captures TurnResourceSnapshot -> CwdSnapshot(repo-b, rev-4)
```

Provider settings、auth 和 custom providers 是 user-global/runtime-global；项目级 settings 不允许声明 custom provider 或覆盖 auth。`ModelSelection` 属于 session state；`ModelGateway` 每次调用通过 user-global `ProviderRegistry` 和 `AuthStore` 解析 provider/auth，不随 cwd 或 focused session 重建。

每个 session 只能有一个 workspace cwd。多个不同 session 可以对应同一个 cwd，并共享该 cwd 的 current `CwdResourceSnapshot`；不同 cwd 拥有独立 `CwdResourceSnapshot`。`CwdResourceSnapshot` 持有构建它时的 `Arc<RuntimeResourceSnapshot>`，并通过 `ResourceOverlayPolicy` 产出 cwd 下的 resolved resource view。cwd/project 资源可以覆盖 same-key runtime/global 资源，例如同名 project skill 覆盖 user-global skill。

资源 reload 按 cwd 处理：成功 reload 会加载一份新的 `CwdResourceSnapshot`，然后在 `ResourceSnapshotStore` 中原子替换目标 `(workspace_id, cwd)` 的 current pointer。已经 running 的 run 不被中途改写，因为它已经用旧 `TurnResourceSnapshot` 构建了 `TurnState`；idle session 和后续 user turn 会 capture 新 snapshot。reload 失败必须保留旧 snapshot 并发布 diagnostics。runtime/global 资源 reload 发布新的 `RuntimeResourceSnapshot`；future turn capture 时如果发现 cwd snapshot 指向旧 runtime revision，则由 `ResourceManager` 懒惰 recompute cwd snapshot。

## 会话打开和聚焦生命周期

打开或聚焦会话不是 UI 页面切换，也不是运行时服务替换。建议顺序：

1. 校验目标会话、workspace 和 cwd；每个 session metadata 中的 cwd 在该 session 生命周期内固定。
2. 调用 `ResourceManager.ensure_cwd_snapshot(CwdResourceRequest { workspace_id, cwd, reason: SessionOpen })`，确保目标 `(workspace_id, cwd)` 存在 current `CwdResourceSnapshot`；不存在时加载 runtime snapshot、cwd-local layer，并通过 overlay policy 构建初始快照。
3. 若目标会话未 loaded，通过 `SessionManager` 创建 `SessionHandle` 并加载新的 `SessionRuntime`，让它记录自己的 `workspace_id` 和 `cwd`。
4. 若目标会话已 loaded，只更新 focused session，不改写其 cwd、phase、queue、current run 或正在 running 的 `TurnState`。
5. 发出 `session_opened`（仅首次 loaded）和 `session_focus_changed`。
6. 需要时发出该 cwd 对应的 `resources_changed` / diagnostics events。
7. 后续 session-scoped 命令路由到对应 `SessionRuntime`；后台 running session 不因失去 focus 被关闭或中止。

## 安全边界

`AgentRuntime` 集中持有 workspace host、资源快照存储和运行时服务入口。凭据和 provider catalog 是 user-global/runtime-global；项目资源只能通过 `ResourceManager` 受 trust gate、source info 和 overlay policy 约束后进入 cwd 的 `CwdResourceSnapshot.resolved`；工具执行仍由 `SessionRuntime` 持有的 session-scoped `Tools` 子系统结合 cwd 和 sandbox view 治理。下游 UI/CLI 只能发送命令和消费事件，不能直接读取本地资源、拼接技能内容、执行工具或写 session 文件。

## Command Route

`ExecuteCommandText` 是 runtime command text 入口，`/...` slash input 只是其中一种语法。它不是 Agent loop 的子步骤：

```text
ExecuteCommandText / ExecuteCatalogCommand
  → AgentRuntime routes target session
  → SessionRuntime.command builds CommandContext + SessionCommandHost
  → CommandManager materialize current catalog
  → parse / resolve_for_execution
  → trusted CommandHandler executes through SessionCommandHost
  → business events and/or command output events
```

只有当 resolved command 是 prompt-like input，例如 `/skill code-review ...`、兼容 `/skill:code-review ...` 或 `/{template}`，才会进入 `SessionRuntime` 的普通 prompt/run pipeline。`/status`、`/usage`、`/model`、`/thinking`、`/reload` 等命令不会直接进入 `Driver`；它们更新 runtime/session state、执行受控 query，或返回 display-neutral command result。

`AgentRuntime` 负责把同一个 `command_id` 贯穿到业务事件和 command output events 中。`CommandAck` 只说明 `ExecuteCommandText` / `ExecuteCatalogCommand` 是否被接收；命令的用户可见结果通过 command output 或业务事件返回。`RuntimeQuery` 不分配 `CommandId`，结果直接返回调用方；query 引用的事实后续变化时由正常业务事件更新或使 UI cache 失效。
