# AgentRuntime

`AgentRuntime` 是 MiniCore 的 UI 无关运行时门面，供下游 CLI、TUI 和 GUI 宿主通过 `AgentRuntimeProtocol` 接入。它不执行单个 Agent turn 的细节，而是管理工作区、运行时服务、事件通道，并通过 `SessionManager` 协调会话生命周期。

## Interface

对下游 adapter 暴露的 interface 保持很小：

```rust
use crate::agent_runtime_protocol as protocol;

pub trait AgentRuntime {
    async fn dispatch(&self, command: protocol::Command) -> Result<protocol::CommandAck, RuntimeError>;
    fn subscribe(&self) -> protocol::EventStream;
    async fn snapshot(&self, session_id: SessionId) -> Result<protocol::Snapshot, RuntimeError>;
}
```

下游 CLI/TUI/GUI 不应该直接调用更细的方法。会话打开、资源刷新、模型切换、工具审批等都通过 `agent_runtime_protocol::Command` 表达。

## 核心职责

- 处理下游 CLI/TUI/GUI adapter 发来的 `agent_runtime_protocol::Command`。
- 发布所有 `agent_runtime_protocol::Event`，维护单调递增的 event sequence，并为后加入的订阅者生成带 `last_event_sequence` 的 `agent_runtime_protocol::Snapshot`。
- 管理工作区，并把会话列表、打开、创建、删除、fork、import 和已加载会话运行时交给 `SessionManager` 协调。
- 通过 `SessionManager` 取得或加载 `SessionRuntime`，再把 session-scoped 命令路由给对应 `SessionRuntime`。
- 基于有效 cwd 创建和重建运行时服务：凭据、设置、模型注册表、模型调用网关、资源加载器、会话管理器和诊断集合。
- 持有 `CommandSurfaceService`，把 runtime builtins、资源命令和后续扩展命令投影成跨 UI 的 command catalog，并按 Parse / Plan / Execute / Present 四阶段处理 `ExecuteSlashCommand`。
- 持有 `RuntimeHookRegistry` 作为内部 runtime service，在资源、slash command、prompt、context、tool、compaction、persistence 和 presentation 等安全点调用 hook，并把 hook 结果交给拥有状态机的模块应用。
- 发布 command presentation events，把 `/status`、`/usage`、`/model`、`/help` 等命令的用户可见结果表达为 message panel 输出、picker、popup、menu、form 或 detail view 请求。
- 在 session 切换、new、fork、import 前后执行受控的 teardown、invalidate、rebind 和 startup 流程。
- 管理跨会话共享的 provider/model catalog、模型调用网关、凭据、全局设置、项目信任和资源刷新入口。
- 保证下游 UI/CLI 不直接接触 Rig、工具实现、凭据、技能文件、会话文件或内部 driver/tool/hook event。

## 运行时服务

运行时服务是绑定到有效工作区的后端依赖集合。典型内容：

```text
RuntimeServices
  ├─ AuthStore
  ├─ SettingsStore
  ├─ ProviderRegistry
  ├─ ModelGateway
  ├─ ResourceLoader
  ├─ CommandSurfaceService
  ├─ RuntimeHookRegistry
  ├─ SessionManager
  ├─ ProjectTrustStore
  └─ RuntimeDiagnostics
```

当用户打开、导入或恢复到不同 cwd 的会话时，`AgentRuntime` 必须重新创建这些服务，避免旧 cwd 的资源、设置、信任状态或凭据解析泄漏到新会话。

`ProviderRegistry` 是 provider/model catalog；`AuthStore` 是凭据解析边界；`ModelGateway` 是唯一真实模型调用入口。三者都属于 `RuntimeServices`，由 `SessionRuntime` 通过引用使用，但不由 `Driver` 持有。完整 provider/model/auth 生命周期见 [ModelGateway](model-gateway.md)。

## 会话替换生命周期

会话替换不是 UI 页面切换，而是运行时所有权迁移。建议顺序：

1. 校验目标会话和工作区。
2. 触发 `SessionBeforeSwitch` / `SessionBeforeFork` 类运行时 Hook。
3. shutdown 旧会话运行时。
4. 执行同步 invalidate 回调，让下游 adapter 清理旧订阅和旧上下文。
5. dispose 旧 `SessionRuntime`。
6. 必要时重建 cwd 绑定运行时服务。
7. 通过 `SessionManager` 创建并加载新 `SessionRuntime`。
8. 加载会话上下文、资源、模型和活跃工具。
9. 通知下游 adapter 重新绑定事件订阅和状态。
10. 发出 `session_opened` / `session_focus_changed` / `diagnostics_runtime_changed` / `resources_changed` 等事件。

## 对齐 pi 的能力

| pi `AgentSessionRuntime` / services | 本项目能力 |
| --- | --- |
| `createAgentSessionServices(options)` | 创建 cwd 绑定运行时服务 |
| `createAgentSessionFromServices(options)` | 从服务创建 `SessionRuntime` |
| `createAgentSessionRuntime(factory, options)` | 创建初始 `AgentRuntime` 状态 |
| `setRebindSession(callback)` | 下游 adapter 重新绑定回调 |
| `setBeforeSessionInvalidate(callback)` | teardown 前同步 adapter 清理点 |
| `switchSession(sessionPath, options)` | `OpenSession` 触发的会话替换流程 |
| `newSession(options)` | `NewSession` |
| `fork(entryId, options)` | `ForkSession` |
| `importFromJsonl(inputPath, cwdOverride)` | `ImportSession` |
| `dispose()` | `CloseSession` / runtime shutdown |

## 安全边界

`AgentRuntime` 集中持有凭据、项目信任、工作区沙箱和运行时服务入口。下游 UI/CLI 只能发送命令和消费事件，不能直接读取本地资源、拼接技能内容、执行工具或写 session 文件。

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
