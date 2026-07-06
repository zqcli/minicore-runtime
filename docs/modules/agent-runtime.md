# AgentRuntime

`AgentRuntime` 是 MiniCore 的 UI 无关运行时门面，供下游 CLI、TUI 和 GUI 宿主通过 `AgentRuntimeProtocol` 接入。它不执行单个 Agent turn 的细节，而是管理工作区、运行时服务、事件通道，并通过 `SessionManager` 协调会话生命周期。

## Interface

对下游 adapter 暴露的 interface 保持很小：

```rust
use crate::agent_runtime_protocol as protocol;

pub trait AgentRuntime {
    async fn dispatch(&self, command: protocol::Command) -> Result<protocol::CommandAck, RuntimeError>;
    fn subscribe(&self) -> protocol::EventStream;
    async fn snapshot(&self) -> Result<protocol::RuntimeSnapshot, RuntimeError>;
}
```

下游 CLI/TUI/GUI 不应该直接调用更细的方法。会话打开、资源刷新、模型切换、工具审批等都通过 `agent_runtime_protocol::Command` 表达。

## 核心职责

- 处理下游 CLI/TUI/GUI adapter 发来的 `agent_runtime_protocol::Command`。
- 发布所有 `agent_runtime_protocol::Event`，维护单调递增的 event sequence，并为后加入的订阅者生成带 `last_event_sequence` 的 `agent_runtime_protocol::RuntimeSnapshot`。
- 管理工作区，并把会话列表、打开、创建、删除、fork、import 和已加载会话运行时交给 `SessionManager` 协调。
- 通过 `SessionManager` 取得或加载 `SessionRuntime`，再把 session-scoped 命令路由给对应 `SessionRuntime`。
- 管理 `WorkspaceServices` 和 `CwdServiceRegistry`，为每个已加载 `SessionRuntime` 解析并 pin 对应的 `CwdScopedServices` generation。
- 持有 `CommandSurfaceService`，把 runtime builtins、资源命令和后续扩展命令投影成跨 UI 的 command catalog，并按 Parse / Plan / Execute / Present 四阶段处理 `ExecuteSlashCommand`。
- 持有 `RuntimeHookRegistry` 作为内部 runtime service，在资源、slash command、prompt、context、tool、compaction、persistence 和 presentation 等安全点调用 hook，并把 hook 结果交给拥有状态机的模块应用。
- 发布 command presentation events，把 `/status`、`/usage`、`/model`、`/help` 等命令的用户可见结果表达为 message panel 输出、picker、popup、menu、form 或 detail view 请求。
- 在 session open、focus、new、fork、import、close 前后执行受控的 open/load/focus/unload 流程；focus 切换不隐式关闭旧 `SessionRuntime`。
- 管理跨会话共享的 workspace host 状态，以及按 cwd/generation 隔离的 provider/model、模型调用网关、凭据、设置、项目信任和资源刷新入口。
- 保证下游 UI/CLI 不直接接触 Rig、工具实现、凭据、技能文件、会话文件或内部 driver/tool/hook event。

`OpenWorkspace` 只建立 workspace 绑定的运行时服务和会话目录，不默认聚焦或恢复任何旧 session。刚打开窗口时 `RuntimeSnapshot.active_session_id = None`、`RuntimeSnapshot.active_session = None`；TUI 可以等用户输入 `/resume` 再列出当前 workspace 的会话，GUI 可以单独调用 `ListSessions` 渲染 sidebar。只有 `OpenSession` / `NewSession` 成功后，`AgentRuntime` 才通过 `SessionManager` 创建或加载 `SessionRuntime`，并在后续 `RuntimeSnapshot.active_session` 中投影该会话的初始 idle 状态。

MVP 的 `AgentRuntime` 嵌入在 CLI/TUI/GUI host 进程内，和 UI host 同生命周期，不作为独立 daemon/server 存活。`subscribe()` / `snapshot()` 的 reconnect 语义用于同一程序上下文内的 late subscribe、reducer/subscriber 重建和 sequence gap recovery；不支持 UI adapter 失败但 runtime 继续运行、随后由新 UI 连接并恢复所有后台 session 的模式。

## 运行时服务

`RuntimeServices` 是内部总称，不是一套会随 focused session 改变而整体替换的全局单例。MVP 采用支持多 session 后台运行的方案 B：`AgentRuntime` 拥有 workspace host 级服务，并维护按 cwd/generation 分桶的服务注册表；每个 `SessionRuntime` 在创建时 pin 一个服务 generation，后续 run 继续使用自己的 generation，直到显式 reload/safe point 或 session unload。

```text
AgentRuntime
  ├─ WorkspaceServices
  │   ├─ EventBus
  │   ├─ SessionManager / SessionIndex
  │   ├─ CommandSurfaceService
  │   ├─ RuntimeHookRegistry
  │   └─ RuntimeDiagnostics
  │
  └─ CwdServiceRegistry
      ├─ key: (workspace_id, cwd, generation)
      └─ CwdScopedServices
          ├─ SettingsView
          ├─ ProjectTrustView
          ├─ AuthView
          ├─ ProviderRegistryView
          ├─ ModelGateway
          ├─ ResourceLoader
          └─ ToolSandboxRoot / ToolGateway inputs
```

打开、导入或恢复到不同 cwd 的会话时，`AgentRuntime` 不重建全局服务，也不迁移正在后台运行的旧 session。它会为目标 session resolve 或创建新的 `CwdScopedServices` generation，然后通过 `SessionRuntimeFactory` 把该 generation 交给新的 `SessionRuntime`。旧 `SessionRuntime` 继续持有旧 generation；只有显式 `CloseSession`、workspace teardown、idle unload 或 shutdown policy 才释放。

`ProviderRegistryView` 是 provider/model catalog 的 cwd-scoped 投影；`AuthView` 是凭据解析边界；`ModelGateway` 是该 generation 内唯一真实模型调用入口。完整 provider/model/auth 生命周期见 [ModelGateway](model-gateway.md)。

资源 reload 也按 cwd/generation 处理：成功 reload 会为对应 cwd 产生新的 service generation。未来 run 使用新 generation；已经 running 的 run 默认继续使用启动时 pin 的 generation，除非进入明确 safe point 并由 `SessionRuntime` 决定切换。

## 会话打开和聚焦生命周期

打开或聚焦会话不是 UI 页面切换，也不是运行时服务替换。建议顺序：

1. 校验目标会话、workspace 和 cwd。
2. 通过 `CwdServiceRegistry` resolve 或创建 `(workspace_id, cwd, generation)`。
3. 若目标会话未 loaded，通过 `SessionManager` 创建 `SessionHandle` 并加载新的 `SessionRuntime`，让它 pin 住该 generation。
4. 若目标会话已 loaded，只更新 focused session，不重建它持有的 service generation。
5. 发出 `session_opened`（仅首次 loaded）和 `session_focus_changed`。
6. 需要时发出该 generation 对应的 `resources_changed` / diagnostics events。
7. 后续 session-scoped 命令路由到对应 `SessionRuntime`；后台 running session 不因失去 focus 被关闭或中止。

## 对齐 pi 的能力

| pi `AgentSessionRuntime` / services | 本项目能力 |
| --- | --- |
| `createAgentSessionServices(options)` | `CwdServiceRegistry.resolve(workspace_id, cwd)` 创建或复用 cwd 绑定 service generation |
| `createAgentSessionFromServices(options)` | 从 pinned `CwdScopedServices` generation 创建 `SessionRuntime` |
| `createAgentSessionRuntime(factory, options)` | 创建初始 `AgentRuntime` 状态 |
| `setRebindSession(callback)` | 下游 adapter 重新绑定回调 |
| `setBeforeSessionInvalidate(callback)` | teardown 前同步 adapter 清理点 |
| `switchSession(sessionPath, options)` | `OpenSession` / `FocusSession` 触发的 open/load/focus 流程；不隐式关闭旧会话 |
| `newSession(options)` | `NewSession` |
| `fork(entryId, options)` | `ForkSession` |
| `importFromJsonl(inputPath, cwdOverride)` | `ImportSession` |
| `dispose()` | `CloseSession` / runtime shutdown |

## 安全边界

`AgentRuntime` 集中持有 workspace host、cwd-scoped service registry 和运行时服务入口。凭据、项目信任、资源和工具沙箱都通过 pinned `CwdScopedServices` generation 进入对应 `SessionRuntime` / run；下游 UI/CLI 只能发送命令和消费事件，不能直接读取本地资源、拼接技能内容、执行工具或写 session 文件。

## Slash Command Route

`ExecuteSlashCommand` 是 runtime control route 的一种入口，不是 Agent loop 的子步骤：

```text
ExecuteSlashCommand
  → CommandSurfaceService.parse
  → CommandSurfaceService.plan
  → AgentRuntime executes plan through backend owner
  → CommandSurfaceService.present outcome
  → command_output_appended / command_interaction_requested
```

只有当解析结果是 prompt-like input，例如 `/skill:{name}` 或 `/{template}`，才会进入 `SessionRuntime` 的普通 prompt/run pipeline。`/status`、`/usage`、`/model`、`/thinking`、`/reload` 等命令不会直接进入 `Driver`；它们更新 runtime/session state，执行 query，或请求 UI 打开交互控件。

`AgentRuntime` 负责把同一个 `command_id` 贯穿到业务事件和 `CommandPresentationEvent` 中。`CommandAck` 只说明 `ExecuteSlashCommand` 是否被接收；slash command 的用户可见结果通过 `command_output_appended` 或 `command_interaction_requested` 返回。
