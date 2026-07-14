# Background Session Runtime Progress

日期：2026-07-06

目的：记录当前关于多 session 后台运行、RuntimeServices scope、Claude-like supervisor 方向的讨论进展，方便后续继续设计。

## 当前已提交状态

已完成并提交的相关文档变更：

- `70da796 docs(runtime): constrain runtime reconnect semantics`
  - 关闭 `BR-002`。
  - 明确当前嵌入式 MVP 中 UI host 与 `AgentRuntime` 同进程、同生命周期。
  - `RuntimeSnapshot.last_event_sequence` 只覆盖同一 host 生命周期内的初始化、late subscribe、reducer/subscriber 重建和 sequence gap recovery。

- `eac94c1 docs(runtime): define multi-session service scopes`
  - 关闭 `BR-003`。
  - 将当前文档收敛为方案 B：一套 `AgentRuntime` 支持多个 `SessionRuntime` 同时 loaded/running。
  - 引入 `WorkspaceServices`、`CwdServiceRegistry`、`CwdScopedServices { workspace_id, cwd, generation }`。
  - 每个 `SessionRuntime` / run pin 自己的 service generation；当时仍保留默认 session 路由，该 selector 后由 BR-034 从 core 删除。

- `2550094 docs(runtime): use resource manager snapshots`
  - 将 `ResourceLoader` 升级为 `ResourceManager` 子系统，并重命名相关 ADR / module 文档。
  - 新增 ADR 0010，接受 per-cwd resource snapshot 作为多 session MVP 的资源隔离方案。
  - 用 `RuntimeResourceSnapshot -> CwdResourceSnapshot -> TurnResourceSnapshot -> StepResourceSnapshot` 取代 `CwdScopedServices` / generation pinning 作为 MVP 核心模型。
  - 明确 reload / ensure / recompose / capture_turn 的线性化边界，running turn 继续使用旧 snapshot，future turn 捕获新 snapshot。
  - 明确 skill body、prompt templates、context files、自定义/追加系统提示词等模型可见资源必须 snapshot 化。

- `fda22a6 docs(prompt): clarify resource prompt boundary`
  - 明确 `ResourceManager` 和 `Prompt` 不直接互调；`SessionRuntime` 是唯一编排者。
  - `ResourceManager` 只产出 captured prompt materials，`Prompt` 只做纯 system prompt 渲染。
  - 补充 `PromptMaterials` 与最终 system prompt 的简短示例。
  - 更新 glossary 和模块总览中的 `提示词素材` / `系统提示词构建器` 边界。

## 当前 dev 修订

分支：`dev`

`design/runtime-resource-snapshot-mode` 已 fast-forward 合并到 `dev`，并已推送到 `origin/dev`。当前文档基线以 `ResourceManager` 级联 immutable snapshots 为准。

本轮讨论进一步收敛了 MiniCore MVP 的目标：UI 和 `AgentRuntime` 是一体进程，不支持 UI detach 后 runtime daemon 继续运行；只要求一个 UI 下多个 session 可以同时各自运行。基于这个约束，BR-003 的服务 scope 设计从 `CwdScopedServices` 修订为 per-cwd resource snapshot 模式。

最新结论：

- 保留 `WorkspaceServices`，作为 event bus、session manager、command surface、hooks、diagnostics、`ResourceManager`、settings/provider/auth/model gateway 的共享容器。
- 删除 `CwdServiceRegistry` / `CwdScopedServices` / service generation pinning 作为 MVP 核心概念。
- 将 `ResourceManager` 定义为运行时内部资源子系统，持有 `ResourceSnapshotStore`、内置 resolver、trust gate、diagnostics、cwd reload pipeline 和 `ResourceOverlayPolicy`；runtime 资源与 `AgentRuntime` 生命周期绑定，`OpenWorkspace` 初始化一次。
- 资源快照分层为 `RuntimeResourceSnapshot -> CwdResourceSnapshot -> TurnResourceSnapshot -> StepResourceSnapshot`；Runtime/Cwd 是 store current snapshots，Turn/Step 不进入 store；MVP 实现前三层，`StepResourceSnapshot` 只定义类型。
- `CwdResourceSnapshot` 持有构建时的 `Arc<RuntimeResourceSnapshot>`，并包含 cwd-local layer 与 overlay 后的 `resolved` effective view。cwd/project 资源可按 `ResourceKey { kind, namespace?, name }` 覆盖 runtime/global 同 key 资源。
- 每个 `SessionRuntime` 固定一个 workspace cwd；多个 session 可以共享同一个 cwd 的 current `CwdResourceSnapshot`。
- 新的显式 user turn / work chain 通过 `ResourceManager.capture_turn(...)` 捕获 `TurnResourceSnapshot`，并用它构建 `TurnState`；automatic retry、overflow recovery、active Steer 和同 `RunId` segment rollover 复用该 snapshot。
- `ReloadResources { workspace_id, cwd }` 为目标 cwd 构建新的 `CwdResourceSnapshot`；成功后通过 `replace_cwd` 原子替换 current pointer；running run 继续使用旧 `TurnState` / 旧 `TurnResourceSnapshot`，下一轮新的显式 user turn 使用新 snapshot。
- provider settings、auth、custom provider 和 `ModelGateway` 都是 user-global/runtime-global；项目级 settings 不允许声明 custom provider 或覆盖 auth/provider endpoint。
- 当前设计暂不考虑热更新、文件监听器或资源发现回调接口；资源更新走显式 cwd reload / startup ensure。全局资源变化通过重建 `AgentRuntime` 生效。
- `ResourceManager` 和 `Prompt` 不直接互调；`SessionRuntime` 在 user turn 启动时捕获 `TurnResourceSnapshot`，从中取得 `PromptResourceView`，再与 `ToolPromptView` 调用 `prompt::assemble_turn(PromptTurnSpec { resources, tools })`。
- `Prompt` 是无状态组装深模块，不读取文件、不读 `ResourceSnapshotStore`、不触发 reload；`ResourceManager` 不构造最终 prompt，只发布 captured resource view。Prompt 生成 immutable `PromptTurn`，并负责 intent 展开与 final model-input projection。
- Command 体系收敛为共享无状态 `CommandManager` + `SessionRuntime` 持有的 session-scoped `Command` facade；`CommandSurface` 只保留为领域总称。
- command manifest 使用多层 JSON，运行时临时 materialize 成 flat catalog；动态命令节点通过 provider + `nodeTemplate` 生成，skill、prompt template、model thinking levels、tools 等都可作为 dynamic command nodes。
- 协议层 `agent_runtime_protocol::Command` 改名为 `agent_runtime_protocol::AgentCommand`；`Command` 短名留给 command 子系统。UI 只能提交 `ExecuteCommandText` 或 `ExecuteCatalogCommand`，不能通过 command result/output 携带完整 runtime mutation command。

修订后的模型：

```text
AgentRuntime
  └─ WorkspaceServices
      ├─ EventBus
      ├─ SessionManager / SessionIndex
      ├─ CommandManager
      ├─ RuntimeHookRegistry
      ├─ RuntimeDiagnostics
      ├─ ResourceManager
      │   ├─ ResourceSnapshotStore
      │   │   ├─ current runtime -> RuntimeResourceSnapshot rev-r
      │   │   ├─ (workspace_id, <root>) -> CwdResourceSnapshot rev-a -> rev-r
      │   │   └─ (workspace_id, <root>/api) -> CwdResourceSnapshot rev-b -> rev-r
      │   └─ ResourceOverlayPolicy
      ├─ user-global ProviderRegistry / AuthStore
      └─ shared ModelGateway

LoadedSessionRuntimes
  ├─ Handle A -> SessionRuntime actor A { cwd = <root> } -> RunTask captures CwdSnapshot(<root>, rev-a)
  ├─ Handle B -> SessionRuntime actor B { cwd = <root> } -> next new work chain captures current CwdSnapshot(<root>)
  └─ Handle C -> SessionRuntime actor C { cwd = <root>/api } -> RunTask captures CwdSnapshot(<root>/api, rev-b)
```

## 已关闭问题

### BR-001

已关闭。`snapshot(session_id)` 已收敛为 `snapshot() -> RuntimeSnapshot`。

当前语义：

- `RuntimeSnapshot` 是 runtime 当前状态读模型。
- 不落盘。
- 不是 UI store。
- 不是 session index。
- 打开 workspace 后默认 `loaded_sessions = []`；BR-034 后 snapshot 在同一水位覆盖所有 loaded runtimes。
- `/resume` handler 与 GUI sidebar 复用 `SessionManager.list_sessions(...)`；GUI/SDK 入口使用 `RuntimeQuery::Session(SessionQuery::List { ... })`。

### BR-002

已关闭，但结论依赖当前嵌入式生命周期假设。

当前语义：

- MVP 不支持 UI adapter 失败/断线但 runtime daemon 继续后台运行再被重连。
- UI host 与 `AgentRuntime` 同生命周期。
- reconnect/resync 只指同一 host 生命周期内的订阅或 reducer 重建。

重要提醒：如果后续转向 Claude-like supervisor / daemon 模式，`BR-002` 需要重新打开，因为 UI 和 runtime 生命周期会重新分离。

### BR-003

已关闭，当前分支按单进程 resource snapshot 模式修订。

历史语义（`eac94c1`）：

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
      └─ CwdScopedServices { workspace_id, cwd, generation }
          ├─ SettingsView
          ├─ ProjectTrustView
          ├─ AuthView
          ├─ ProviderRegistryView
          ├─ ModelGateway
          ├─ ResourceManager
          └─ ToolSandboxRoot / Tools inputs

LoadedSessionRuntimes
  ├─ SessionRuntime A -> pins CwdScopedServices(repo-a, gen-1)
  ├─ SessionRuntime B -> pins CwdScopedServices(repo-b, gen-2)
  └─ SessionRuntime C -> pins CwdScopedServices(repo-a, gen-1)
```

这个方案解决的是：多个 `SessionRuntime` 虽然天然隔离 messages / phase / queue / current_run，但不天然隔离 resources / settings / auth / trust / tool cwd / sandbox。`CwdScopedServices` 用于把这些 cwd-sensitive 依赖固定到 session/run。

最新修订：在当前产品约束下，resources 是唯一需要 cwd 维度快照化的运行输入；settings/auth/provider/model gateway 均为 user-global/runtime-global。`CwdScopedServices` 过重，已被 `ResourceManager` + 级联 immutable snapshots + user-turn/work-chain `TurnResourceSnapshot` capture 取代。多 session 隔离重点变为：session state 独立、session cwd 固定、`CwdResourceSnapshot.resolved` 提供 cwd effective resource view、新的显式 user turn / work chain 启动时捕获 `TurnResourceSnapshot`、reload 只影响 future work chain、工具副作用另由 tool policy/sandbox/mutation strategy 处理。

## 当前新讨论方向

用户提出：如果要模仿 Claude Code 做多进程隔离 session，是否可以让 `AgentRuntime` 作为 supervisor 管理多个 session runtime 和其他资源进程？

当前讨论结论：可以，而且这是一个与当前单进程方案 B 不同的演进方向。

推荐概念模型：

```text
UI / TUI / GUI
  │
  │ IPC / RPC
  ▼
AgentRuntime Supervisor Process
  ├─ SessionCatalog / SessionIndex
  ├─ ProcessRegistry
  ├─ EventBus / EventMultiplexer
  ├─ ApprovalRouter
  ├─ WorktreeManager
  ├─ AuthBroker / TrustPolicy
  ├─ SessionWorker A process
  ├─ SessionWorker B process
  └─ SessionWorker C process
```

每个 `SessionWorker` 进程内部：

```text
SessionWorker Process A
  ├─ SessionRuntime A
  ├─ SessionHandle / SessionStorage
  ├─ ResourceManager A
  ├─ Prompt builder A
  ├─ ModelGateway client A
  ├─ Tools A
  ├─ Driver A
  └─ cwd / worktree A
```

这个模型更接近 Claude Code：

- `AgentRuntime` 变成 supervisor。
- 每个顶层 session 是一个完整 worker process。
- UI 可以 attach / detach / peek / reply。
- session 继续后台运行不依赖当前 terminal。
- cwd/resources/auth/trust/sandbox 隔离主要由 process + cwd/worktree + per-session effective config 完成。

## Claude Code 调研结论

Claude Code 明确支持多个顶层 background sessions，不是 subagents。

公开文档中的关键点：

- `claude agents` 是所有 background sessions 的统一视图。
- 每个 background session 是完整 Claude Code conversation。
- background session 可以在没有 terminal attached 的情况下继续运行。
- 有单独 supervisor process 管理后台 session。
- agent view 可查看 session state：Working / Needs input / Idle / Completed / Failed / Stopped。
- session 可以 attach / detach；detach 不停止 session。
- 后台 session 编辑文件前会进入独立 git worktree。
- worktree 通常在 `.claude/worktrees/` 下。

Claude Code 的资源隔离主要来自：

```text
supervisor process
  ├─ Session process A + cwd/worktree A + transcript A + effective config A
  └─ Session process B + cwd/worktree B + transcript B + effective config B
```

分离维度：

- session state：每个 background session 是完整 conversation。
- cwd/file edits：每个后台 session 进入独立 worktree。
- resources：按 session cwd/worktree 解析 `CLAUDE.md`、`.claude/settings`、agents、hooks、skills/MCP 等。
- trust：workspace trust 与目录相关。
- permissions：Managed/User/Project/Local 多层 scope，结合当前 working directory 和 permission mode 生效。
- sandbox：Bash sandbox / write access boundary 约束工具执行。
- auth：credential store 多为用户级，但具体使用受 session effective config / trust / permission / sandbox 约束。

## Codex 调研结论

Codex 与 Claude Code 不完全相同。

公开文档明确支持：

- `codex resume`
- `codex fork`
- session transcript
- `codex app-server`
- `codex --remote`
- `--cd`
- `--add-dir`
- `--sandbox`
- `--ask-for-approval`
- subagent workflows / multi-agent threads

但公开文档中没有像 Claude Code 那样明确描述：

- 一个统一后台 session view。
- supervisor process 托管多个 detached top-level sessions。
- 每个 top-level background session 自动 worktree 隔离。

更稳妥的判断：

- Codex 支持 session 持久化、恢复、fork、remote app-server 和多进程/多 invocation 并发。
- 顶层 detached background session 的产品化模型不如 Claude Code 明确。
- Codex 的隔离更像每个 invocation/session 固定 `--cd`、sandbox、approval policy。

## pi 调研结论

pi coding-agent 当前更像单 active session 的 terminal coding harness。

它支持：

- `/resume`
- `/new`
- `/fork`
- `/clone`
- `/import`
- `/tree`
- message queue
- session 持久化和切换

但没有看到明确的多个顶层 session 同时后台运行模型。

pi 的生产路径更接近：

```text
AgentSessionRuntime
  └─ current AgentSession
       └─ Agent / agent-loop
```

切换 session 时是 session replacement / rebind 语义：

```text
switchSession
  → 关闭或替换当前 AgentSession
  → 创建新的 cwd-bound services
  → 创建新的 AgentSession
  → UI 重新绑定
```

因此：

- pi 适合参考 session replacement、cwd-bound service creation、session persistence。
- 但如果 MiniCore 要做多个顶层 background sessions，不能直接照搬 pi 的 current-session replacement 模型。

## 当前架构分叉

### 路线 A：单进程 resource snapshot 模式（已由 ADR 0022 收窄）

下图是讨论期形态；其中单 workspace 跨 `repo-a` / `repo-b` 的示例已被 ADR 0022 否决。当前 MVP 应读作同一 canonical root 及其子目录：

```text
AgentRuntime process
  └─ WorkspaceServices { root = <root> }
      ├─ LoadedSessionRuntimes
      │   ├─ Handle A -> SessionRuntime actor A { cwd = <root> }
      │   ├─ Handle B -> SessionRuntime actor B { cwd = <root> }
      │   └─ Handle C -> SessionRuntime actor C { cwd = <root>/api }
      ├─ ResourceManager
      │   └─ ResourceSnapshotStore
      │       ├─ current runtime -> RuntimeResourceSnapshot
      │       ├─ (workspace_id, <root>) -> CwdResourceSnapshot
      │       └─ (workspace_id, <root>/api) -> CwdResourceSnapshot
      └─ shared ModelGateway / settings / auth / provider registry
```

优点：

- 实现较轻。
- 不需要 IPC。
- 适合嵌入式 runtime repo。
- 当前文档已经收敛到此方向，并已合并进 `dev`。

缺点：

- 需要仔细避免共享状态污染。
- 需要严格维护 resource snapshot capture / reload / recompose 的线性化边界。
- crash boundary 较弱。
- 与 Claude Code 的 supervisor/background UX 不完全一致。

### 路线 B：Claude-like 多进程 supervisor

```text
AgentRuntime Supervisor process
  ├─ SessionWorker process A
  ├─ SessionWorker process B
  └─ SessionWorker process C
```

优点：

- 隔离强。
- 每个 session 有独立 cwd/worktree/process state。
- 更适合真正后台运行、detach/attach、agent view。
- crash recovery 和资源污染边界更清晰。

缺点：

- 需要 IPC/RPC。
- 需要 worker process lifecycle。
- 需要 event multiplexing。
- 需要 approval routing。
- 需要重新设计 UI reconnect / RuntimeSnapshot 覆盖后台 sessions。
- 会改变 `BR-002` 当前关闭时依赖的生命周期假设。

## 建议的演进方式

不要立即推翻当前方案 B。建议把未来多进程能力作为 host adapter 演进，而不是重写核心概念。

可以引入抽象：

```rust
trait SessionRuntimeHost {
    async fn dispatch(&self, command: SessionCommand) -> Result<SessionAck>;
    fn subscribe(&self) -> SessionEventStream;
    async fn snapshot(&self) -> SessionSnapshot;
    async fn shutdown(&self, policy: ShutdownPolicy) -> Result<ShutdownReport>;
}
```

先实现：

```text
InProcessSessionRuntimeHost
```

未来扩展：

```text
ProcessSessionRuntimeHost
```

这样 `AgentRuntime` 上层仍然管理 session catalog / events / focus / command routing，但具体 session runtime 可以从 in-process 迁移到 process worker。

## 对现有设计的影响

如果继续当前单进程 resource snapshot 模式：

- `WorkspaceServices` 仍然合理。
- `CwdServiceRegistry` / `CwdScopedServices` 不再作为 MVP 核心概念。
- `ResourceSnapshotStore` 按 cwd 保存 current immutable resource snapshot。
- `BR-002` 保持关闭。
- `BR-003` 保持关闭，但处理记录以当前分支修订为准。

如果改为 Claude-like supervisor：

- `AgentRuntime` 需要变成 supervisor process。
- 每个顶层 session 建议是独立 `SessionWorker` process。
- `CwdScopedServices` 可弱化为 worker-local services，或保留为 worker 内部概念。
- 需要新增 `WorktreeManager`。
- 需要新增 `ProcessRegistry` / `SessionRuntimeHost`。
- 需要重新打开或新增 issue：UI detach/reconnect 后如何恢复所有 background sessions 的可视状态。
- `RuntimeSnapshot` 可能需要升级为 supervisor snapshot / session catalog + per-session status projection。

## 下轮建议继续讨论的问题

1. MiniCore 的目标到底是嵌入式 runtime core，还是 Claude-like local supervisor daemon？
2. 多 session 后台运行是否必须支持 UI 关闭后继续运行？
3. 是否默认为每个后台 session 创建 worktree？
4. 如果不默认 worktree，如何防止多个 session 同时修改同一工作树？
5. `AgentRuntime` 是否应该抽象 `SessionRuntimeHost`，让 in-process 和 process-worker 共存？
6. Auth / trust / permissions 是放在 supervisor，还是 worker 内部解析？
7. ModelGateway 是 worker 内部直接调用 provider，还是通过 supervisor 的 `AuthBroker` / `ModelGatewayProxy`？
8. 如果采用 supervisor，多 session 状态如何进入 `RuntimeSnapshot`？是否重开 BR-002？
9. 如果未来引入 worker process，当前 `WorkspaceServices` / `ResourceManager` / `ResourceSnapshotStore` 哪些保留为 supervisor shared service，哪些下沉为 worker-local service？

## 当前建议结论

短期建议：以当前分支的单进程 resource snapshot 模式作为 MiniCore MVP 文档基线。

中期建议：先实现 `WorkspaceServices + ResourceSnapshotStore + LoadedSessionRuntimes` 的纵切，验证同一 UI 下多个 session 同时 running、reload 只影响 future turn、run 使用启动时捕获的 resource snapshot。

长期如果要模仿 Claude Code：让 `AgentRuntime` 演进为 supervisor，管理多个 `SessionWorker` process、worktree、event multiplexing、approval routing 和 attach/detach。届时需要重新处理 UI reconnect / RuntimeSnapshot 范围问题，并重新评估是否需要 worker-local services。

## Tool 子系统设计收敛记录

日期：2026-07-07

背景：pending tool approval 的恢复语义已落入正式文档：每个 loaded session 当前 run 的待审批工具调用投影到对应 `RuntimeSnapshot.loaded_sessions[*].current_run.pending_tool_approvals`，`ToolApprovalBroker` 继续持有冻结的 `prepared_args`。下列内容已进一步收敛到 `docs/modules/tools.md`、`docs/modules/driver.md`、`docs/modules/session-runtime.md` 和 `docs/modules/agent-runtime-protocol.md`。

### Approval request lifecycle

建议在现有 `session_id + run_id + call_id` 之外新增稳定 `ApprovalRequestId`，让 UI、日志、去重和 stale response 处理更明确。

`PendingToolApprovalView` 后续可扩展为：

```rust
pub struct PendingToolApprovalView {
    pub approval_id: ApprovalRequestId,
    pub session_id: SessionId,
    pub run_id: RunId,
    pub call_index: ToolCallIndex,
    pub call_id: ToolCallId,
    pub tool_name: String,
    pub risk: ToolRisk,
    pub reason: String,
    pub preview: ToolApprovalPreview,
    pub created_at: Timestamp,
}
```

`DecideToolApproval` 后续可同时携带 `approval_id` 与三元组，broker 必须校验它们仍匹配同一个 pending approval。批准后只能执行 broker 内部冻结的 `prepared_args`。

### Duplicate / stale decision handling

重复审批必须有明确语义，避免 UI 双击、快捷键重复触发或旧 snapshot 里的按钮导致工具重复执行。

建议定义 outcome：

```rust
pub enum ApprovalDecisionOutcome {
    Accepted,
    AlreadyResolved { original_decision: ToolApprovalDecision },
    StaleRun,
    StaleCall,
    NotFound,
}
```

约束：

- 同一个 `approval_id` 只能 resolve 一次。
- approve 已处理后再次 approve，不得再次执行工具。
- approve 后再 reject，应返回 stale/already-resolved。
- run aborted / finished 后到达的 decision 应返回 stale。
- session close / broker cleanup 后到达的 decision 应返回 not found 或 stale。

### Approval modes and remembered grants

需要同时支持：每次询问、一次性审批后后续免询问、完全无审批模式。

建议新增：

```rust
pub enum ToolApprovalMode {
    AskEveryTime,
    UseRememberedGrants,
    AutoAllow { max_risk: ToolRisk },
    AutoDeny { reason: String },
}

pub enum ToolApprovalDecision {
    ApproveOnce,
    ApproveGrant { scope: ApprovalGrantScope, ttl: Option<Duration> },
    Reject { reason: Option<String> },
}

pub enum ApprovalGrantScope {
    SameCallFingerprint,
    SameToolInRun,
    SameToolInSession,
    SameToolInWorkspace,
}
```

建议新增 `ToolApprovalGrantStore`，不要把长期授权塞进 `ToolApprovalBroker`。职责划分：

```text
ToolApprovalBroker
  管 pending approval、冻结 prepared_args、等待用户回答

ToolApprovalGrantStore
  管 approve once / same call fingerprint / same run / same session / same workspace
```

grant key 至少应包含：

```rust
pub struct ApprovalGrantKey {
    pub tool_name: ToolName,
    pub cwd: PathBuf,
    pub risk: ToolRisk,
    pub args_fingerprint: Option<String>,
    pub sandbox_profile: Option<String>,
}
```

`SameCallFingerprint` 必须使用 canonical args hash，避免批准一个低风险参数后放行同工具的高风险参数。

### AutoAllow constraints

完全无审批模式只跳过 UI approval，不跳过工具治理。

仍必须执行：

- active tool check。
- schema validate。
- workspace trust / sandbox path check。
- enterprise hard deny。
- mutation queue / resource conflict check。
- audit log / diagnostics。

`AutoAllow { max_risk }` 开启时，UI snapshot 或 diagnostics 应能显示当前处于无审批模式，尤其是 `bash`、`write`、`edit`、`apply-patch` 这类高风险工具。

### Tool batch scheduler

建议新增 `ToolExecutionCoordinator`，统一执行已声明的并行、串行、mutation exclusive 和 chain 顺序约束；它不根据工具名自行发明策略。

```rust
pub struct ToolBatchItem {
    pub call_index: ToolCallIndex,
    pub invocation: ToolInvocation,
    pub execution_mode: ToolExecutionMode,
}

pub enum ToolExecutionMode {
    ParallelSafe,
    OrderedByCallIndex,
    MutatingExclusive { resource_key: ResourceKey },
    ChainStep { chain_key: ChainKey },
}
```

建议默认策略：

- `read` / `grep` / `find` / `ls`：`ParallelSafe`。
- `write` / `edit` / `apply-patch`：`MutatingExclusive`。
- `bash`：默认 `OrderedByCallIndex` 或按 cwd/resource 进入 `MutatingExclusive`。
- chain 工具：同一个 `chain_key` 内严格串行，不同 `chain_key` 可并行。

### Execution order and LLM feedback order

工具可以并行执行，但回填给 Rig / LLM 的结果必须稳定有序。

示例：

```text
LLM 返回顺序:
  call[0] read
  call[1] grep
  call[2] edit

执行完成顺序:
  grep -> read -> edit

回填给 Rig / LLM:
  result[0] read
  result[1] grep
  result[2] edit
```

建议在 `ToolInvocation` / `ToolInvocationResult` 或 driver-local batch item 中保留 `call_index`，最终：

```rust
results.sort_by_key(|r| r.call_index);
AgentRun::tool_results(results);
```

UI 事件可以按实时完成顺序展示；session message 与回填给 LLM 的 tool result 顺序应按 `call_index` 或 provider/Rig 要求固定。

### Approval and scheduler interaction

审批等待需要纳入调度器，而不是 executor 前的临时分支。

建议规则：

- `ParallelSafe`：可并行请求 approval，也可并行执行。
- `MutatingExclusive`：按 `call_index` 请求 approval；批准并执行完成后再处理下一个 mutation。
- `ChainStep`：同 chain 前一步 approved + finished 后，才进入后一步 approval / policy / executor。

这样可以避免用户批准一个 preview 后，前面的 mutation 已经改变文件，导致 preview 与实际执行环境不一致。

### Review invariants

后续 review tool 子系统时重点检查：

- 任意 `ToolExecutor.execute(...)` 之前必须完成 schema、policy、approval/grant。
- UI 永远不能修改 `prepared_args`。
- `approval_id` 只能 resolve 一次。
- resolved / aborted / finished / session close 后 pending 必须清理。
- grant 必须有 scope、fingerprint、ttl 或 revoke 入口。
- 并行执行允许乱序完成，回填 LLM 必须按 `call_index` 稳定排序。
- mutation 和同 chain 工具必须串行。
- `AutoAllow` 仍受 hard deny、sandbox、audit 约束。

当前建议：统一采用 session-scoped `Tools` 子系统，由 `SessionRuntime` actor 保留 tool control/approval ownership 和 RuntimeSnapshot pending projection，active `RunTask` 经 `DriverHost::invoke_tool_batch(...)` 与 run-only `ToolBatchInvoker.invoke_batch(...)` 执行工具。`Tools` 内部封装 `ToolPolicy`、只持冻结 record/waiter 的 `ToolApprovalBroker`、`ApprovalRequestId`、`ToolApprovalGrantStore` 和 `ToolExecutionCoordinator`，以吸收 Codex/pi request lifecycle 与并发调度经验。

## Tools 子系统文档落地

本轮已将口头设计正式落入模块文档：

- `Tools` 是 `SessionRuntime` 内部的 session-scoped 工具子系统，不是独立 runtime，也不是 UI 工具层。
- `SessionRuntime` 协调 `Driver` 和 `Tools`；`Driver` 只通过 `DriverHost::invoke_tool_batch(...)` 请求工具批量结果，不直接依赖 `Tools`。
- 历史记录：当时曾把直接 `impl DriverHost for SessionRuntime` 视为合法简化版。该结论已由 [ADR 0021](../adr/0021-session-runtime-separates-actor-control-from-run-execution.md) 修订：生产实现必须使用 per-session actor + run-scoped `RunTask` + owned `SessionDriverHost`，不能让完整 `drive_run().await` 长期借用或锁住 actor state；fake host 仍可用于纯 Driver 单测。
- `ToolBatchInvoker.invoke_batch(...)` 是 active run 的工具治理和执行入口；actor-owned `Tools` 负责 registry/active/prompt/control/approval，invoker 只携带 immutable execution profile、approval broker 和 execution kernel。
- 新增 ADR 0011 固化该决策：`Tools` 是 `SessionRuntime` 内部的 session-scoped 子系统，文档和实现不再使用 gateway 作为架构术语。
- 本地执行策略不由 LLM tool call 决定；provider parallel 参数只影响模型是否一次返回多个 tool calls。MiniCore 默认 parallel；session config 或任一 tool definition 要求 sequential 时整批串行；并发执行可乱序完成，但结果必须按 `ToolCallIndex` 稳定回填。
- `ToolPolicy` 保持纯判断器；preview 构造、path canonicalization、sandbox check 和 schema validation 由 `ToolInvocationPlanner` 负责。
- pending approval 使用 `ApprovalRequestId` 去重和防 stale；长期免批使用 `ToolApprovalGrantStore`，与 `ToolApprovalBroker` 分离。

## BR-005 暂停语义收敛进展

本轮 grilling 已确认：`paused` 不应表示 run terminal status，而应表示 current run 在可恢复 checkpoint 上 `Suspended`，等待后续 resume 继续同一个未完成的 AgentRun / tool-result continuation。

设计方向：

- `RunTerminalStatus` 只保留 `Completed`、`Failed`、`Aborted`。
- 新增 `CurrentRunState::{Running, WaitingApproval, Suspended { resume_id, reason }}`。
- 新增 `RunEvent::Suspended` / `RunEvent::Resumed`，并保留 `RunEvent::Finished` 作为唯一终态事件。
- 典型 suspend checkpoint 包括：tool result 已产生但尚未回填 Rig / provider、等待用户交互、external job pending、用户在 safe point 主动暂停。ADR 0019 后 resume state 只在同一 host 生命周期内存活，host shutdown 不恢复 running run。
- 客户端 session selection 变化不是暂停；pending approval 在 MVP 中是 current run 的 waiting substate，不等同于跨生命周期 suspend。

已同步完成：`agent-runtime-protocol.md`、`agent-runtime-events.md`、`driver.md`、`adr/0003-agent-runtime-events-use-event-msg-and-lifecycle-pairs.md` 和 `system-blueprint-review-issues.md` 已按该语义更新，BR-005 已标记为 Resolved。

## BR-008 Driver 输入 seam 收敛进展

本轮 grilling 已确认：BR-008 的核心不是 `DriverHost` / `SessionDriverHost` 是否存在，而是 `DriveRequest` 不应携带完整 `TurnState`。`DriverHost` 限制的是 driver 回调外部能力时能访问什么；`DriveRequest` 限制的是 driver 一开始被喂进来什么。只要 `DriveRequest { turn_state: TurnState }` 存在，`Driver` 就仍可见 `TurnResourceSnapshot`、resource revision、context usage 和工具治理状态，seam 仍然过宽。

设计收敛为：`SessionRuntime` 继续拥有完整 `TurnState`，run 启动时只投影 `DriverTurnInput` 放进 `DriveRequest`。ADR 0017 进一步把该类型收窄为 `model`、原子 `PromptCallProfile`、`thinking_level` 和 `stream_options`；system prompt 与 active tool schemas 不再独立 patch。`TurnResourceSnapshot`、完整 `PromptTurn`、resource revision、context usage、queue/storage 或工具治理状态不跨 seam；turn resources 留在 per-run `SessionDriverHost` 中。

已同步完成：`driver.md`、`session-runtime.md`、`model-gateway.md`、`resource-manager.md`、`skills.md`、ADR 0011、ADR 0013、`CONTEXT.md` 和 `system-blueprint-review-issues.md` 已按该语义更新，BR-008 已标记为 Resolved。由于 `CONTEXT.md` 同时新增了 `TurnState` 词条，BR-017 也已标记为 Resolved。

## BR-009 ModelGateway 实现顺序收敛进展

本轮 grilling 已确认：BR-009 的风险来自阶段 5 需要真实 `DriverHost::call_model(...)`，但阶段 9 才准备完整 `ModelGateway`。如果阶段 5 为了跑通 text-only driver 写临时 provider/auth/usage/error 路径，阶段 9 会被迫拆掉重写。

设计收敛为：提前实现最小稳定 `ModelGateway` spine，而不是把阶段 9 的全部能力提前。阶段 4 改为 `Rig Driver + ModelGateway seam spike`，固定 `ModelSelection`、`ModelCallPurpose`、`ModelCallRequest`、`ModelCallResult`、`ModelCallErrorKind`、`ModelCallUsage` shape、`ModelGateway.call_model(...)`、最小 `ProviderRegistry.resolve(...)` 和 `AuthStore.resolve(...)`。阶段 5 的 text-only driver 只能走 `DriverHost::call_model -> ModelGateway.call_model(...)`，不允许 `Driver` / `SessionDriverHost` match provider、直读 env 或构造 Rig provider client。阶段 9 只在既有 spine 上扩展 custom provider、完整 auth、fallback、usage normalization 和 context usage。

已同步完成：`implementation-roadmap.md`、`model-gateway.md`、ADR 0014、`CONTEXT.md` 和 `system-blueprint-review-issues.md` 已按该语义更新，BR-009 已标记为 Resolved。

## BR-010 Hook owner 和实现顺序收敛进展

本轮 grilling 已确认：BR-010 的问题不是缺少某个 hook，而是 `SessionRuntime` 与 `ModelGateway` 同时声明模型/provider hook owner，导致 provider payload、安全能力、错误策略和 diagnostics 归属不清。

设计收敛为：hook owner 遵循 runtime 边界，谁拥有安全点业务不变量，谁调用 hook、应用 typed result、重新校验并记录 diagnostics。`SessionRuntime` 拥有 run/prompt/context/queue/compaction/persistence 安全点；`Tools` 拥有工具治理安全点；`ModelGateway` 拥有 model/provider 边界安全点；`CommandManager` / session `Command` 拥有 command catalog/resolve/output 安全点。`Driver` 不调用 hook，`RuntimeHookRegistry` 只保存 handler 和策略。

实现顺序也同步收敛：当前 MVP 不实现 hook system，也不在 roadmap 中设置 `RuntimeHooks MVP` 阶段。后期第一批 hook 才考虑接入已经稳定的 owner 流程，例如 `BeforeAgentStart`、`PromptBuilt`、`ContextProjection`、`ToolBeforePolicy`、`ToolAfterExecute`、`SessionBeforeCompact`、`AfterSessionCommit` 和 `CommandOutputBuild`。provider hooks 后期仍由 `ModelGateway` 拥有，raw provider payload patch 默认不开放。

已同步完成：`runtime-hooks.md`、`session-runtime.md`、`model-gateway.md`、`driver.md`、`implementation-roadmap.md`、`agent-runtime.md`、`architecture.md`、`modules/README.md`、ADR 0008、ADR 0015、`CONTEXT.md`、`system-blueprint-review-issues.md` 和 `system-blueprint-review-issues-round2.md` 已按该语义更新，BR-010 已标记为 Resolved。由于同一 owner 分层同时固定了 future registry / session-scoped view / fixed cwd trust 判定，BR-035 也已标记为 Resolved。

## BR-011 / BR-013 模型调用 purpose 与压缩请求边界收敛进展

本轮 grilling 已确认：`UsagePurpose` 与 `ModelCallPurpose` 表达的是同一模型调用事实，不应存在转换层；并且 `Retry` / `Background` 属于 attempt、run control 或调度状态，不是调用业务目的。设计收敛为只保留 `ModelCallPurpose { AgentRun, CompactionSummary }`，并固定 `ModelCallRequest.purpose -> ModelCallUsage.purpose -> future SessionEntry::Usage.purpose` 原样传播。provider fallback/retry 使用 `ModelCallAttempt`，session/run retry 使用 `RetryReason` / `DriveEntry::Retry` / future call lineage；后台 session 的正常调用仍是 `AgentRun`。

同时确认 `SummaryModelRequest` 造成了 Compaction 与 ModelGateway 的第二套请求边界。该类型改为纯 `CompactionSummaryMaterial { system_prompt, messages, max_output_tokens }`：`Compaction` 只生成摘要内容和输出预算，`SessionRuntime` 选择摘要模型、thinking/stream policy、分配 correlation id，并构造唯一 `ModelCallRequest { purpose: CompactionSummary, tools: [], max_output_tokens: Some(...) }`；`ModelGateway` 继续负责 provider/auth/fallback/usage/error/cancellation。

已同步完成：`model-gateway.md`、`usage-stats.md`、`session-manager.md`、`compaction.md`、`implementation-roadmap.md`、`modules/README.md`、ADR 0014、`CONTEXT.md` 和 `system-blueprint-review-issues.md` 已按该语义更新，BR-011 与 BR-013 已标记为 Resolved。

## BR-014 Running compact 延后执行语义收敛进展

本轮确认：手动 `/compact` 不能隐式 abort 当前 run，但 running 时也不必直接拒绝。该语义现统一命名为 `CommandRunPolicy::QueueAfterRun`：idle 时立即执行；存在 active run、waiting approval、suspended run 或立即 retry chain 时，由 `SessionRuntime` 保存唯一 `PendingSessionAction::Compact { command_id, instructions }`，当前 work 不受影响。早期 `CommandPhasePolicy::DeferUntilPostRun` 名称已由 ADR 0016 替换。

pending compact 是结构化 post-run action，不是 follow-up/next-turn message，不进入模型上下文。它通过 `queue_updated.pending_actions` 和 `QueueSnapshot.pending_actions` 暴露；当前 work chain terminal handling（required stable commit if any）和 terminal facts 完成后，在 queued steering continuation / follow-up continuation 之前执行；`NextTurn` 不自动启动并可保留到 settled 后的下一次显式 prompt。required overflow recovery / immediate retry 优先，manual compact 优先于 threshold auto compaction；manual 已执行时跳过重复 auto compaction。`AbortRun`、`ClearQueue`、session close 或 shutdown 清除 pending compact。

已同步完成：`agent-runtime-protocol.md`、`agent-runtime-events.md`、`command-surface.md`、`session-runtime.md`、`compaction.md`、`implementation-roadmap.md`、`architecture.md`、`modules/README.md`、`CONTEXT.md` 和 `system-blueprint-review-issues.md` 已按该语义更新，BR-014 已标记为 Resolved。

## BR-028 Prompt delivery 与 command run policy 收敛进展

本轮参考 pi 与 Codex 的运行中输入分流后确认：不建立通用 `InputSchedule` 服务。模型可见输入统一使用 `PromptDelivery { Steer, FollowUp, NextTurn }`，删除独立 `Steer` / `FollowUp` / `NextTurn` protocol command；普通 prompt、skill、prompt template 和 prompt-producing slash command 全部归一到 `SessionRuntime.admit_prompt_intent(...)`。

slash/catalog command 自身使用正交的 `CommandRunPolicy { Immediate, IdleOnly, QueueAfterRun }`。`/status` 等 query 在 active work 中立即执行并异步输出到 message panel；`/compact` 映射为 typed `PendingSessionAction::Compact`；`/skill` 等 prompt-producing command 先 resolve 成结构化 prompt intent，再按调用方的 `PromptDelivery` 调度。raw slash text 不进入消息队列或 pending action。

已同步完成：`CONTEXT.md`、ADR 0016、`agent-runtime-protocol.md`、`agent-runtime-events.md`、`agent-runtime.md`、`command-surface.md`、`session-runtime.md`、`compaction.md`、`runtime-hooks.md`、`architecture.md`、`modules/README.md`、`implementation-roadmap.md` 和 round2 review issue。BR-028 已标记为 Resolved。

## Prompt 子系统收敛进展

本轮在 BR-015 / BR-016 和 PromptDelivery 基础上继续 grilling，确认旧的“`prompt.rs` 只返回 system prompt 字符串、SessionRuntime 手工展开 skill/template、Driver 分别接收 system prompt/tool schemas”会把最终模型输入组装分散到多个 owner。设计已收敛为无状态 Prompt 深模块，而不是 workspace-global `PromptManager` 或长期 `ContextManager`。

当前结论：

- `SessionRuntime` 是 Pull Master：在新的显式 user turn / work chain boundary 捕获 `TurnResourceSnapshot`，取得 `PromptResourceView` 与独立 `ToolPromptView`，调用 `prompt::assemble_turn(PromptTurnSpec { resources, tools })` 创建 immutable `PromptTurn`。
- `PromptResourceView` 是所有非工具稳定 Prompt 输入的唯一 seam，复用 ResourceManager canonical `ResourceKey`、source info、content hash、cwd revision 和 fingerprint；Prompt 不建立第二套 registry、overlay 或 reload 逻辑。
- `PromptTurn.resolve_intent()` 是 skill/template 正文进入 user message 的唯一组装入口。active Steer 使用 active `PromptTurn`；FollowUp、NextTurn 和 idle submission 使用 future `PromptTurn`。
- system prompt 与 active tool schemas 绑定为原子 `PromptCallProfile`，跨 Driver seam 只能整体替换；`DriverTurnInput` 不再暴露两个独立 patch 字段。
- 每次模型调用区分 durable history、protected current input、CurrentRun context 和 CurrentCall context；最终统一生成 `ModelInputProjection`，并校验 tool call/result、required contribution、dedup、budget、persistence 和 fingerprint。
- Compaction 只重建 durable history；current input late-bind 到压缩后的 history，不能在首次调用前被摘要。
- 动态 RAG/memory/IDE context 使用带 source、content hash、persistence 和 requirement 的 typed `ContextMaterial`。项目文件、skills/templates 仍必须经 ResourceManager snapshot。
- `before_next_model_call` 返回组合式 `NextModelCallPlan`，可同时处理 persistent steer、完整 prompt profile、model options、CurrentRun/CurrentCall context 和 abort/suspend 控制。
- 当前不实现 Prompt cache、Context graph 或 ContextManager。未来只有出现多个异步 context provider、跨 call working set、动态 token budget 和后台 distillation 后，才考虑不拥有 durable history 的 session-scoped `ContextWorkspace`。

新增 `docs/modules/prompt-templates.md` 关闭 BR-016；新增 ADR 0017 记录 immutable turn assembly 与拒绝长期 Manager 的取舍。旧 `fda22a6` 中“Prompt 是纯 system prompt builder”的边界已被本轮设计深化：纯计算和不反向读取 ResourceManager 的原则保留，但 Prompt 的职责扩展为完整的 intent/system/context/model-input 组装 seam。

后续清理删除了已明显滞后的集中式 `docs/implementation-roadmap.md`。实现顺序不再作为行为事实来源；各模块文档、AgentRuntimeProtocol、AgentRuntimeEvents 和 ADR 是长期权威。BR-020 的 assistant lifecycle 由事件文档固定；BR-021 最初把 `persistence_save_point` 收敛为 write-batch barrier，随后由 ADR 0019 进一步简化为统一 `SessionWriter.commit(...)`：公共 save-point event 删除，稳定领域事实在 commit 成功后发布。text-only run 提交 `UserInput` 与 `AssistantFinal`，完整 tool round 单独提交。BR-024 通过 stable-unit policy 关闭：partial assistant、pending approval 和 incomplete tool round 不持久化，abort/failure/crash 只恢复此前 committed batches，不做 synthetic repair。

BR-022 随后完成术语纯度清理。本轮仅提升文档表达，不改变架构：当前流程全部改用 MiniCore 自身术语解释；pi、Codex、LangChain 等只保留为明确标注的设计参考对象，不构成兼容承诺。外部历史类名和私有调用链已从架构入口与模块现行流程删除；ADR 继续保存必要的历史取舍。

BR-023 通过 ADR 0018 关闭。`AgentRuntime` 现在明确提供 Command、Query、Event 和 Snapshot 四种能力；新增按 runtime/session/settings/resources/command surface/models/usage/diagnostics 分组的可扩展 `RuntimeQuery` 框架。query 只读并直接返回 `QueryResponse`，不经过 `CommandAck` 或 event stream；GUI sidebar、settings、资源详情和 usage 等程序化读取使用 Query，`/status` / `/usage` 等用户命令继续使用 command output。

BR-024 通过 ADR 0019 关闭。所有 session mutation 统一走 `SessionWriter.commit(SessionWriteBatch)`，成功返回是可信写入结果；不引入 `SessionRevision`。writer 只接受 `UserInput`、完整 `ToolRound`、`AssistantFinal`、`Compaction`、独立 `SessionMutation` 和 `TreeMutation` 等稳定单元。公共 `persistence_save_point` 删除，commit 成功后才发布对应领域事件；running 中的 partial assistant、pending approval 和 incomplete tool round 不落盘，abort/failure/crash 只保留最后 committed batches。

BR-027 按 run identity 边界关闭。`RunId` 只标识已经通过 `run_started` 公开的一次 `Driver::drive_run()`，在 runtime host 内全局唯一；`AbortRun { run_id }` 保持不变，由 loaded runtime current-run lookup 路由。`Turn + current_run = None` 是不可取消的有界 admission/finalization 窗口，模型/provider/tool/approval 等长等待必须发生在 `run_started` 后。UI 只在拿到 run id 后展示 run-level abort，也可以在 adapter 本地暂存提前的 Ctrl-C intent。

BR-030 删除公开 `WaitForIdle`，且不新增 core `wait_until_settled()`。`session_settled` 是 observer 消费的领域事实；UI 使用 snapshot/event reducer，CLI/RPC/test 如需等待则在 adapter/test-support 层用 subscribe-before-dispatch 或已有 reducer state 提供 helper，不承诺任意 background session late join。runtime 内部需要 idle 后执行的动作继续使用 phase guard、`QueueAfterRun` 和 typed pending action；未来若出现独立 server 的明确需求，只评估绑定具体 RunId 的 transport-level join。

BR-031 明确放弃 runtime 层“abort 后归还队列文本给编辑器”的行为。`AbortRun` 清除尚未消费的 steering/follow-up 和 pending action、保留 `NextTurn`，并在 queue state 实际变化时只发布清理后的完整 `queue_updated`；已经 commit 的 steer 保留在 durable history。`ClearQueue` 才清除全部 queue state。runtime 不增加 `returned_to_editor`、removed delta 或 editor action，具体 UI 可用本地 submission history 和 undo state 实现 best-effort restore，该体验不属于 core 或 reconnect contract。

BR-033 统一事件坐标所有权：`workspace_id`、`session_id`、`run_id`、`command_id` 只由外层 `Event` 权威携带，所有 `EventMsg` 删除仅用于 routing/correlation 的副本；局部对象 identity 和 tree transition operands 保留。diagnostics 的 runtime/workspace/session/run 归属也改读外层，`DiagnosticSubject` 只表达 tool call、resource path 或 model 等细粒度对象。adapter 必须消费完整 `Event`，脱离 dispatch stack 的组件构造包含 event metadata 与 payload 的本地 view；内部 publisher 使用 routing-profile constructor 阻止非法坐标组合。

BR-034 删除 core 的 focused/current/active session selector。`FocusSession`、`session_focus_changed` 和 `LoadedSessionRuntimes.focused_session_id` 不再存在；所有 session-scoped command 显式携带 `SessionId`，客户端 selected session 只属于 adapter。后端状态只保留 persistent catalog、loaded residency、`SessionPhase`、optional `CurrentRunState` 和 settled fact。`RuntimeSnapshot` 改为同一全局 event 水位下的 `loaded_sessions: Vec<SessionSnapshot>`，resources 和 command catalog 进入各自 session projection；未 loaded session 继续通过 query 读取。

## 2026-07-13 Round3 Follow-up

本阶段继续处理 `docs/review/system-blueprint-review-issues-round3.md` 中的 Open issues。已完成并提交以下决策：

- `71fabce docs(review): defer BR-049 after Rig steer validation`
  - 核对 Rig 0.40.0、Pi、Codex、OpenClaw 和 Attractor 后，确认 Rig `AgentRun` / `AgentRunStep` 没有阻止 MiniCore 落地的架构级障碍。
  - Steer 在完整 assistant/tool turn 后、下一次模型调用前交付；Rig 不支持公开 mid-run history append 时，同一 MiniCore `RunId` 可以顺序 rollover 多个私有 Rig segment。
  - FollowUp 仍在当前 work chain 完成后启动新公开 run。版本 pin、usage/tool-name 映射、`NeedsResolution`、`preresolved_result` 和 conformance tests 延后到真实 Driver integration spike。

- `b982104 docs(review): defer BR-050 and gate shell sandbox`
  - MVP 只承诺 MiniCore builtin executor 的进程内 path authorization；路径 canonicalization、symlink/父目录检查和 denied roots 不能冒充通用子进程 sandbox。
  - `read/grep/find/ls` 可按内置只读工具实现推进；`write/edit/apply-patch` 在安全文件操作、审批、abort 和 mutation queue 闭合后可单独启用。
  - 通用 `bash` 在 MVP disabled。后续 `Sandboxed` shell 必须由满足请求 policy 的 OS-native/external backend 强制，能力不足返回 `SandboxUnavailable` 并 fail closed；`FullAccessWithApproval` 明确没有 sandbox guarantee。

- `bcfd109 docs(review): defer BR-051 to pre-development contract review`
  - 跨模块 seam 的调用方向和 ownership 已基本确定，但约 15 个字段形状、sink 方法集、typed error 和行为不变量仍未闭合。
  - 先完成其余 review issues；任何生产代码纵切开始前重新打开，按 protocol/command、driver/model、tools、resources、compaction/dynamic-context 分组执行全仓 interface contract closure review。
  - 每个被引用类型必须得到“定义、删除/合并、或转入带明确 owner/阶段的后续 issue”之一，Rig/provider 私有类型不得泄漏到上层。

- `65c28d5 docs(review): defer BR-052 to protocol contract review`
  - `UserInput`、`StreamOptions`、`ThinkingLevel` 与 BR-051 联合进入开发前 protocol contract review；需统一明确 wire 形状、默认值、持久化/恢复边界、生效时机、capability clamp 和 provider mapping owner。
  - `ResumeRun` 纳入 MVP command reachability review，默认移入 post-MVP，不为保留死表面凭空增加 `SuspendRun`。
  - `NewSession { workspace_id, cwd: Option<PathBuf> }` 及 cwd canonical/root containment 已由 ADR 0022 关闭。

- `b8444bf docs(review): close BR-053 context-limit recovery path`
  - 每次 CallModel 前的 `prompt::project_model_call(...)` 是最终 projection validation；本地超限返回 `PromptError::ContextLimitExceeded`，不调用 provider。
  - Driver 将本地 `PromptProjection` 和 provider `ContextOverflow` 归一为 `DriverError::ContextLimitExceeded { source, ... }`，继续使用 `DriveResult::Failed`，保留 diagnostics/usage 来源差异。
  - `SessionRuntime` 在当前 run 先发布 `run_finished { failed }` 后进入独立 `Compaction` phase；整个 work chain 最多一次 overflow compact-and-continue，成功后用新 `RunId` 和 `DriveEntry::Continue { reason: ContextOverflowRecovery }` 继续，再次超限或无可压缩内容时 fail closed。
  - pre-run threshold gate 只在 bounded admission 内同步判断；命中后先切换 `Turn → Compaction` 再执行 summary model / future ProviderNative 外部工作，压缩完成后才首次分配 Agent `RunId`。
  - MVP compaction method 仍为 portable `SummaryModel`。后期 `ProviderNative` 可调用 GPT 类专用 compact endpoint；model-bound 加密 artifact 只持久化一次，由同 provider adapter 注入后续请求，不进入普通 message/event/snapshot，具体 envelope/compatibility fields 延后到 ProviderNative integration design。

### BR-054 / ResourceManager 最终决策

本段保留上方历史分析的上下文，但旧暂停点已 superseded。BR-054、BR-057 和 BR-062 已在权威文档中关闭，最终决策如下：

- runtime 与 UI/`AgentRuntime` 生命周期绑定。`RuntimeResourceSnapshot` 在 `OpenWorkspace` 初始化一次；MVP 无 runtime reload、runtime current-pointer 替换、runtime-version drift 或 lazy cwd recompose。全局资源变化通过重建 `AgentRuntime` 生效。
- cwd reload 保留；`ResourceSnapshotStore::replace_cwd(...)` 是唯一资源 current-pointer 替换线性化点。`capture_turn` 与 `replace_cwd` 按读取 current pointer 的时点线性化。
- 保留 `RuntimeResourceSnapshot -> CwdResourceSnapshot -> TurnResourceSnapshot -> StepResourceSnapshot`。Runtime/Cwd 是 store current snapshots；Turn/Step 不进入 store。
- Resource 定义扩展为：对模型运行有影响、在所属 scope 内稳定、可冻结为 immutable projection 的输入。原始 owner/mutation owner 不变；ResourceManager 只冻结 owner-produced prompt-safe projections，绝不反向读取 `ModelState`、`Tools`、`AuthStore`、`SettingsStore` 或 provider owner handles。
- `TurnResourceSnapshot` 直接字段为 `session_id`、`user_turn_id`（不是 `RunId`）、`Arc<CwdResourceSnapshot>`、behavior、model、environment、policy、fingerprint；不再引入额外 turn view wrapper 或 turn prompt snapshot 类型。
- Runtime scope 放 product/user-global defaults；Cwd scope 放 project/cwd/trust/catalog overlay；Turn scope 放 behavior/model/environment/policy。Environment MVP 只含 workspace root、fixed cwd、platform、date/time/timezone、interaction capabilities，不含 VCS I/O。Step MVP reserved。
- `PromptResourceView` 是所有非工具稳定 Prompt 输入的唯一 seam，暴露 materials/behavior/model/environment/policy/skill/template/fingerprint；`ToolPromptView` 保持 Tools 独立。Prompt 入口为 `prompt::assemble_turn(PromptTurnSpec { resources, tools }) -> PromptTurn`。
- 新显式 user turn / work chain capture 一次 `TurnResourceSnapshot`。automatic retry、overflow compaction recovery、active Steer、同 `RunId` Rig segment rollover复用；FollowUp、NextTurn、new idle prompt 新 capture。Resource reload 只影响新的显式 user turn / work chain。
- MVP `Turn` 中继续拒绝 model/thinking/stream/active-tools/profile mutation。后期 safe-point mutation 必须通过 `StepResourceSnapshot` 或明确 step override，并原子替换 `PromptCallProfile` 与 future `ToolBatchInvoker`，fingerprint 一致。
- `capture_turn` 在稳态只读 current cwd pointer 并冻结传入 typed projections；`OpenWorkspace/OpenSession/NewSession` 保证 ensure，不再存在 turn capture 兜底读盘或 lazy recomposition。
- Runtime 不使用递增 revision；`CwdResourceRevision` 用于 reload；`TurnResourceFingerprint` 组合 cwd revision + behavior/model/environment/policy versions。具体 canonical 算法仍归 BR-063。

旧分析中关于 `TurnPromptSnapshot` / 多 owner prompt views / runtime reload 的建议均为历史记录，不代表现行设计。

### BR-055 延伸调研：消息构建流程审计与 Transcript-First 重设计建议

本节记录 2026-07-13 对 MiniCore 整套消息构建流程的重新审计。它由 BR-055 的 `ResolvedPromptInput.parts/messages` 问题触发，但调研范围已扩展到 `SessionStorage -> SessionRuntime -> Prompt -> Driver/Rig -> ModelGateway -> provider` 的完整链路。

本节是工作进度和推荐方案，不是已接受 ADR。现行权威文档仍以 `prompt.md`、`session-runtime.md`、`driver.md`、`session-manager.md` 和 `model-gateway.md` 为准；若采纳推荐方案，需要新增 ADR 并同步修订这些文档，不能只关闭 BR-055。

#### 1. 调研问题与结论修正

最初 BR-055 把问题表述为：`ResolvedPromptInput.parts` 如何折叠成 `messages` 未定义，可能导致持久化内容与模型可见输入分叉。

调研确认：

- `parts` 和 `messages` 可以是不同表示层，它们不需要结构相等。
- `ResolvedPromptInput.messages` 不是历史消息。`resolve_intent()` 只接收当前 `PromptIntent`，没有 history 参数；历史由 `SessionHandle.build_session_context()` 单独构建。
- 当前最合理的语义是：
  - `parts`：当前 intent 解析后的有序、typed、resource-resolved input IR。
  - `messages`：同一当前输入 lower 后的 current user message(s)。
  - `contribution_stamps`：当前输入的 provenance/fingerprint contribution，不是另一组模型消息。
- BR-055 的核心仍成立，但应改写为：

> `ResolvedPromptInput` current-input 的 canonical lowering、cardinality、layout 与 exact-reuse contract 未定义。

准确的不变量不是“整个 UserInput batch 等于完整模型输入”，因为完整模型输入还包含 system prompt、history、tools 和 transient context；准确表述应为：

```text
成功 committed 的 current user message
  == 首次模型调用中对应的 current input message
  == active Steer commit 后交给下一 segment 的同一 message
```

相等表示 role、content、content-part 顺序和模型可见正文相同；storage 生成的 entry id、parent id 和 timestamp 不属于该内容相等关系。

#### 2. Lowering 的含义

`lowering` 是本次分析使用的设计术语，不是仓库当前已定义的方法名。它指把语义丰富、MiniCore-specific 的高层表示转换为更通用、更接近执行层的表示：

```text
PromptIntent
  -> resolved PromptInputPart[]
  -> canonical session user message
  -> provider-neutral model message
  -> provider wire DTO
```

例如：

```text
PromptInputPart::SkillBlock { metadata, full_body }
PromptInputPart::Text("重点检查并发")
  ->
MessageRecord::User(
  "<skill ...>\n...full body...\n</skill>\n\n重点检查并发"
)
```

这不是压缩或丢失信息。真正的 modality，例如 image/file，可以继续作为 typed content part；provider 不理解的 `SkillBlock`、`SelectedCode`、`ResourceBlock` 则应在 Prompt implementation 内稳定渲染为 text 或其它明确的 model-facing part。

#### 3. MiniCore 当前消息类型的实际语义

当前 `ResolvedPromptInput` 草图：

```rust
pub struct ResolvedPromptInput {
    pub parts: Vec<PromptInputPart>,
    pub messages: Vec<MessageRecord>,
    pub contribution_stamps: Arc<[PromptContributionStamp]>,
}

pub enum PromptInputPart {
    Text(String),
    Image(ImageAttachment),
    File(FileAttachment),
    SelectedCode(SelectedCode),
    SkillBlock(ResolvedSkillBlock),
    ResourceBlock(ResolvedResourceBlock),
}
```

真实场景映射：

| `PromptInputPart` | 场景 | 预期内容 |
| --- | --- | --- |
| `Text` | 普通输入、additional instructions、template 渲染文本 | 用户正文 |
| `Image` | 截图、设计稿、报错图片 | immutable/durable image content or reference、media type、hash |
| `File` | PDF、日志、源文件附件 | immutable/durable file content or reference、name、media type、hash |
| `SelectedCode` | IDE 选中代码 | path、range、language、实际 code、hash |
| `SkillBlock` | 显式 `/skill` / `InvokeSkill` | captured skill metadata、完整正文、content hash |
| `ResourceBlock` | 显式 resource/context/template expansion | resource key、source、展开正文、content hash |

这里的 parts 应当是已经解析、已经 pin 住正文的输入，不应保存稍后重新读文件的可变 path/reference。

当前 `MessageRecord` 草图：

```rust
pub enum MessageRecord {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
    Custom(CustomMessage),
    BranchSummary(BranchSummaryMessage),
    CompactionSummary(CompactionSummaryMessage),
}
```

它是 MiniCore session-domain canonical record，用于持久化、恢复、UI projection、compaction 和运行时上下文。它不是任何 provider 直接接受的标准协议。

当前设计中需要区分四个 messages 概念：

| 位置 | 语义 |
| --- | --- |
| `ResolvedPromptInput.messages` | 当前 intent lower 后的 current user message(s) |
| `SessionContext.messages` | 从 committed session path 重建出的 effective durable history |
| `TurnState.messages` | 当前 work chain pin 住的 history baseline |
| `ModelInputProjection.messages` | history/current/transient 最终排列后的模型可见序列 |

字段都叫 `messages`，但生命周期和 owner 不同，已经构成明显的认知负担。

#### 4. 当前历史 owner 与构建流程

MiniCore 没有 Codex 风格的长期 `ContextManager`。当前 durable history 的 owner 是：

```text
SessionStorage       // committed session tree / batches
SessionWriter        // trusted mutation seam
SessionHandle        // domain facade
```

主要接口：

```rust
impl SessionHandle {
    pub async fn commit(
        &self,
        batch: SessionWriteBatch,
    ) -> Result<CommittedSessionBatch, SessionWriteError>;

    pub async fn build_session_context(
        &self,
    ) -> Result<SessionContext, SessionError>;
}
```

`build_session_context()`：

- 只读取成功 committed 的 stable batches。
- 沿 current leaf 的 root-to-leaf path 构建。
- 保留完整 user/assistant/tool-call/tool-result 协议关系。
- 应用最新 compaction，输出 summary + retained suffix + messages after compaction。
- 遇到不完整 committed tool round 或非法 path 时 fail closed。

当前预期调用链：

```text
SessionStorage
  -> SessionHandle.build_session_context()
  -> SessionContext.messages
  -> TurnState.messages
  -> Driver/Rig history
  -> Prompt final projection
```

但是 `TurnState.messages -> Driver/Rig history` 这一步目前没有被接口闭合。

#### 5. 当前消息构建流程审计

当前 user input 从 UI 到 provider 大致经过：

```text
UI draft
  -> UserInput / command
  -> PromptIntent
  -> ResolvedPromptInput.parts
  -> ResolvedPromptInput.messages
  -> SessionWriteBatch::UserInput / SessionEntry::Message
  -> protected current_input lane
  -> DriveEntry::Prompt.messages
  -> Rig prompt/history
  -> ModelCallProjectionInput.current_input
  -> ModelInputProjection.messages
  -> ModelCallRequest.messages
  -> provider DTO
```

约有 10 至 11 个表示层。

当前 durable history 大致经过：

```text
SessionEntry / StoredSessionBatch
  -> SessionContext.messages
  -> TurnState.messages
  -> Rig history
  -> ModelCallProjectionInput.durable_history
  -> ModelInputProjection.messages
  -> ModelCallRequest.messages
  -> provider DTO
```

转换多本身不是错误；真正的问题是多个中间表示都看起来像 source of truth，且部分转换没有唯一 owner。

#### 6. 严重问题清单

##### P0：历史 seed 没有进入 Driver

当前：

```rust
pub struct TurnState {
    pub messages: Vec<MessageRecord>,
    // ...
}

pub struct DriveRequest {
    pub entry: DriveEntry,
    pub turn: DriverTurnInput,
    // no history
}

pub enum DriveEntry {
    Prompt { messages: Vec<MessageRecord> },
    Continue { reason: ContinueReason },
    Resume { serialized_run: SerializedAgentRun },
}
```

调用流程把 `DriveEntry::Prompt.messages` 当 current input 使用；`DriverTurnInput` 只含 model/profile/options；`SessionDriverHost` 只含 resources/tools/gateway/link。`TurnState.messages` 没有实际跨到 Driver。

Driver 文档随后假定 Rig 产生：

```text
AgentRunStep::CallModel { prompt, history, turn }
```

但初始 `history` 如何 seed 到 Rig 未定义。实现者可能：

- 漏掉历史；
- 让 Driver 反向读取 SessionStorage；
- 把 current input 同时放进 history 和 prompt，造成重复；
- 把完整 `TurnState` 扩大进 Driver seam。

##### P0：`parts/messages` 双表示没有 canonical lowering contract

当前未定义：

- 谁是 source of truth；
- 一个 intent 产生一条还是多条 user message；
- part 顺序、separator、empty text、adjacent text merge；
- `SkillBlock` / `SelectedCode` / `ResourceBlock` 如何渲染；
- image/file 与 text 是否处于同一 user message；
- hook 修改 current input 后如何重新 canonicalize；
- commit 与首次模型调用是否复用同一个 lower 结果。

`SessionWriteBatch::user_input(message)` 明确是 one user message，而 `ResolvedPromptInput.messages` 是 `Vec<MessageRecord>`，cardinality 也未对齐。

##### P0：Rig history 与 committed history 双轨

`SessionStorage` 是 durable truth；Rig `AgentRun` 也持有 prompt/history/full_history 作为 protocol working state。当前已规定 tool round commit 成功后才能把 results 喂回 Rig，这是正确的，但还应钉死：

```text
Rig history 不是 durable source of truth。
模型返回的未提交 assistant draft 可以先推进 Rig 到 CallTools / Done，这是协议状态机正常工作所必需的。
完整 ToolRound commit 成功前，tool results 不得喂回 Rig，也不得发生下一次模型调用。
未提交 assistant/tool draft 不得成为 retry、compaction recovery 或新 run 的 history seed。
commit failure 后当前 run 必须终止，不能继续使用 Rig 内存状态。
compaction/recovery 后必须从 SessionStorage 重建，不 resume 旧 Rig history。
```

##### P1：current input lane 生命周期过长

当前 user input 先 commit，再为了 pre-run compaction protection 被从 durable history 排除，并作为 `current_input` late-bind。这在 Driver 启动前是有价值的。

但当前设计把 `durable_history/current_input` 区分继续带到每一次模型调用。调研认为这超出实际需要：

- pre-run gate 完成并创建 Rig segment 后，current input 已经是 live transcript 尾部的普通 user message。
- 工具后续 call 应看到 `history + current user + assistant tool call + tool results` 的一个有序 transcript。
- active Steer commit 后成为新的 transcript tail，而不是长期维持第二条 current lane。

##### P1：`MessageRecord` 泄漏到 ModelGateway

当前：

```rust
pub struct ModelInputProjection {
    pub messages: Arc<[MessageRecord]>,
    // ...
}

pub struct ModelCallRequest {
    pub messages: Vec<MessageRecord>,
    // ...
}
```

`MessageRecord` 含 `Custom`、`BranchSummary`、`CompactionSummary` 等 session-domain 语义。若直接交给 `ModelGateway`，每个 provider adapter 都可能重复决定这些 variant 是否进入模型、使用什么 wrapper 和 role。

应增加 provider-neutral、model-facing 的窄类型，例如 `ModelMessage`，并由 Prompt final projection 统一执行：

```text
MessageRecord
  -> ModelMessage
  -> OpenAI / Anthropic / Gemini DTO
```

`ModelGateway` 只负责最后一步 provider encoding，不负责理解 MiniCore session/display 语义。

##### P1：`ModelInputProjection` 与 `ModelCallRequest` 重复

两者都保存 system/messages/tools/output contract，Driver 机械复制。若没有结构约束，validated projection 可能在复制时丢字段或被改写。

候选收敛方向：

```rust
pub struct ModelCallRequest {
    pub input: ModelInputProjection,
    pub model: ModelSelection,
    pub purpose: ModelCallPurpose,
    pub thinking_level: ThinkingLevel,
    pub stream_options: StreamOptions,
    // correlation / limits
}
```

##### P2：MVP 预建过多 future context/hook 契约

当前已经设计：

- `CurrentRun` / `CurrentCall` / `Durable` context scopes；
- required/optional unavailable contribution；
- `PromptBuilt`、`ContextProjection`、`BeforeNextModelCall`、`BeforeRunFinish`、`BeforeModelCall`、`BeforeProviderPayload` 等多个 future safe points；
- contribution stamps 在多个层次重复携带。

但 MVP 不实现 RuntimeHooks，也没有 RAG/memory/context producer。建议只保留 safe-point owner 与禁止事项，删除 MVP interface 中没有真实 producer 的字段级 patch contract。

#### 7. 同类项目对照

##### Codex

Codex 的 `ContextManager` 是 live model-visible history owner：

```rust
pub(crate) struct ContextManager {
    items: Vec<ResponseItem>,
    // ...
}

pub(crate) fn for_prompt(
    mut self,
    input_modalities: &[InputModality],
) -> Vec<ResponseItem> {
    self.normalize_history(input_modalities);
    self.items
}
```

每次 sampling 前：

```rust
sess.clone_history()
    .await
    .for_prompt(&turn_context.model_info.input_modalities)
```

用户输入、assistant output、function call 和 function output 都先记录到 history；下一次 sampling 使用完整 normalized effective history。normalization 负责补/删孤立工具项、处理 modality；compaction 直接替换 live history projection。base instructions 和 tools 独立于 history。

启发：模型调用前只有一个 effective transcript owner；current prompt 只在进入 history前是增量，不作为整个 run 的长期平行 history lane。

##### Gemini CLI

Gemini CLI 使用 `AgentChatHistory` 作为 strong owner，当前 request 是 `PartListUnion`，转换成 `{ role: user, parts }` 后追加/late-bind 到 history。每次 API 调用发送：

```text
curated/managed previous Content[]
  + current user Content
```

context management 开启时使用 `apiHistoryOverride`，它仍然是一次完整的 effective `Content[]`，只不过已经由 history graph 做过 summary/dedupe/masking。工具协议严格保持：

```text
model(functionCall)
  -> user(functionResponse)
```

IDE context injection 会在 pending functionCall 时暂停，避免切断 call/response adjacency。

启发：current request 可在 history management 阶段 late-bind，但进入模型调用后仍是一个完整 ordered transcript；工具 pair 是 layout 的硬不变量。

##### pi coding-agent / pi-agent-core

pi 使用 `Agent.state.messages` 作为 live transcript：

```text
runAgentLoop(prompts, context)
  -> currentContext.messages = context.messages + prompts
```

每次模型调用：

```text
context.messages
  -> transformContext
  -> convertToLlm
  -> { systemPrompt, messages, tools }
  -> provider
```

assistant tool call 和 tool results 追加到同一 `currentContext.messages`；下一模型调用再次转换完整 context。`convertToLlm` 是 session-domain/custom messages 到 model-compatible messages 的唯一 seam，例如：

- `compactionSummary` -> user-role summary text；
- `branchSummary` -> user-role summary text；
- `bashExecution` -> user text or excluded；
- `custom` -> user message；
- standard user/assistant/toolResult pass through。

compaction 后通过 session projection 重建并整体替换 `agent.state.messages`。

启发：一个 live transcript + 一个 model conversion seam，认知成本显著低于多条长期 lane。

##### Claude Code / Anthropic

Claude Code 内部请求组装未公开，不应声称具体实现细节。公开文档确认：

- 每个 session 把消息、tool use/result 持久化为本地 JSONL；
- context window 包含 conversation history、file/command outputs、CLAUDE.md、skills 和 system instructions；
- 接近窗口上限时先清理旧工具输出，再 summarize conversation；
- compaction 后继续使用 summary + retained useful context。

Anthropic provider wire 使用自己的 Messages/tool-use blocks；这再次说明 runtime canonical message 与 provider DTO 必须分层。

##### Provider cache / continuation

prompt cache、Anthropic `cache_control`、OpenAI/Codex `previous_response_id`、sticky WebSocket 等只是传输/计费优化，不应改变 runtime 的逻辑上下文模型。

pi 的 Codex adapter 只有在“当前完整 input 是上一轮 request + response baseline 的严格前缀延伸”时，才发送 `previous_response_id + delta`；条件不满足就回退完整 input。这说明正确方向是：

```text
runtime 先构造完整逻辑上下文
provider adapter 再选择 full payload 或等价 continuation/delta
```

#### 8. 调研过的替代方案

##### 方案 A：actor-owned single effective conversation

引入 `EffectiveConversation`，由 `SessionRuntime` actor 持有唯一 live transcript；每次模型调用通过 actor/RunLink 投影 request。删除长期 current lane、`TurnState.messages`、Driver profile/history ownership，甚至可以把 Prompt 能力合并进 Conversation deep module。

优点：

- 概念最简单；
- current input、Steer、tool round commit 后直接进入同一 transcript；
- model request 只有一个投影来源。

主要风险：

- Rig `AgentRun` 仍持有 protocol history，形成 actor transcript 与 Rig working state 双份 live state；
- 每次 call 回 actor 准备完整 request，扩大高频 RunLink 路径；
- 若完全忽略 Rig history，必须先验证 Rig sans-IO API 能安全工作；
- 方案中过度删除 `PromptTurn` / `ToolProfileBaseline` 会损失已经闭合的资源/工具 fingerprint 不变量。

结论：方向有价值，但全量采用风险过高，必须先做 Rig spike。

##### 方案 B：保留 lanes，修复 seed 和类型

保留 `durable_history/current_input/transient_context`，新增 `AgentRunSeed { durable_history, current_input }`，明确 current input entry IDs，增加 `ModelMessage`，其余 Prompt/Driver/SessionRuntime 分工基本不变。

优点：

- 迁移风险低；
- current input pre-run protection 明确；
- 容易适配 Rig 的 `{ history, prompt }` 接口。

缺点：

- 大部分现有认知复杂度仍然存在；
- current input 在整个 run 中继续作为长期 lane；
- Rig rollover、Steer、tool continuation 仍需维护 lane 迁移规则。

结论：适合作为保守 fallback，不是首选终局。

##### 方案 C：每次 CallModel 从 committed storage projection

只认 SessionStorage 为权威；每次 Driver CallModel 前通过 RunLink 获取 `ConversationSnapshot`，不让 `TurnState` 保存 messages。

优点：

- durable source of truth 最强；
- compaction/Steer/retry 后天然读取最新 committed context；
- 避免 actor 手工同步 `TurnState.messages`。

缺点：

- 每次 model call 增加 mailbox round trip；
- JSONL 全量 rebuild 不可接受，必须引入 cache/watermark/delta；
- `ConversationCache` 本身重新引入复杂状态；
- Driver/Rig working history 仍需与 snapshot 对齐。

结论：一致性强但实现成本过高，不符合当前 MVP。

#### 9. 推荐方案：Transcript-First、run-scoped live projection

综合三个方案，推荐采用中间路线：

> SessionStorage 是 durable truth；一次 `DriveRequest` 接收完整 `ConversationSeed`；Driver/Rig 持有 run-scoped live transcript projection；只有 commit 成功的 exact delta 才能推进该 live transcript；每次模型调用从同一个 transcript 做 model-facing projection。

目标架构：

```text
SessionStorage / SessionWriter
  -> SessionHandle.build_session_context()
  -> ConversationSeed
  -> Driver private Rig adapter / run-scoped transcript
  -> prompt::project_model_call(conversation + transient overlay)
  -> ModelInputProjection<ModelMessage>
  -> ModelGateway
  -> provider DTO
```

这保留 MiniCore 比 Codex/Gemini/pi 更强的 stable commit 不变量，同时采用它们“一个 live effective transcript”的核心模式。

#### 10. 推荐核心类型

##### Canonical session message

保留 `MessageRecord` 作为 session-domain canonical record。它服务 storage/UI/compaction/resume，不直接作为 provider wire。

##### Current input resolution

建议删除公开 `parts + messages` 双状态，收敛为：

```rust
pub struct ResolvedPromptInput {
    user_message: UserMessage,
    contribution_stamps: Arc<[PromptContributionStamp]>,
    fingerprint: PromptInputFingerprint,
}
```

`PromptInputPart` 可以作为 Prompt implementation 内部 IR；若需要 debug/query，可暴露只读 projection，不让调用方重新 fold。

MVP 一个 `PromptIntent` 固定产生一条 user message，与 `SessionWriteBatch::user_input(message)` 对齐。若未来需要一个 submission 产生多条 user message，必须另行定义 batch grouping、event grouping 和 compaction protection，不能仅因字段是 `Vec` 就默认允许。

##### Conversation seed

```rust
pub struct ConversationSeed {
    pub messages: Arc<[MessageRecord]>,
    pub fingerprint: ConversationFingerprint,
}
```

seed 是已经应用 compaction、已经包含本次 committed user input 一次、协议完整的有序 transcript。

##### Drive request

```rust
pub struct DriveRequest {
    pub run_id: RunId,
    pub session_id: SessionId,
    pub conversation: ConversationSeed,
    pub turn: DriverTurnInput,
    pub limits: DriveLimits,
}
```

`DriverTurnInput` 继续保持窄接口，只含 model/profile/options；conversation 独立于它，避免回退成 `DriveRequest { turn_state }`。

`DriveEntry::Prompt { messages }` 可删除或仅保留 entering mode，不再承担历史/current message 容器职责。

##### Provider-neutral model message

```rust
pub enum ModelMessage {
    User {
        parts: Arc<[ModelContentPart]>,
    },
    Assistant {
        text: Arc<str>,
        tool_calls: Arc<[ModelToolCall]>,
    },
    ToolResult {
        call_id: ToolCallId,
        parts: Arc<[ModelContentPart]>,
        is_error: bool,
    },
}

pub enum ModelContentPart {
    Text(Arc<str>),
    Image(ModelImage),
    File(ModelFile),
}
```

Prompt final projection 统一处理：

- `CompactionSummary` -> `ModelMessage::User(Text(summary wrapper))`；
- `BranchSummary` -> model-visible user summary or excluded by explicit policy；
- `Custom` -> include/exclude and role mapping；
- `User` / `Assistant` / `ToolResult` -> validated model messages；
- tool call/result pairing and order；
- unsupported session-only messages -> diagnostic/error, not provider-specific guess。

##### Model call projection

推荐删除长期 current lane：

```rust
pub struct ModelCallProjectionInput<'a> {
    pub profile: &'a PromptCallProfile,
    pub conversation: &'a [MessageRecord],
    pub transient_context: &'a [ContextMaterialContribution],
    pub output_contract: Option<&'a OutputContract>,
}

pub struct ModelInputProjection {
    pub system_prompt: Arc<str>,
    pub messages: Arc<[ModelMessage]>,
    pub tools: Arc<[ToolSchema]>,
    pub output_contract: Option<OutputContract>,
    pub diagnostics: Arc<[PromptDiagnostic]>,
    pub contribution_stamps: Arc<[PromptContributionStamp]>,
    pub fingerprint: ModelInputFingerprint,
}
```

`CurrentRun` / `CurrentCall` context vectors可合并成一个 `transient_context`，scope 放在 item 内部。MVP 没有 producer 时保持空，不扩大外部 seam。

##### ModelCallRequest

建议直接持有 projection：

```rust
pub struct ModelCallRequest {
    pub session_id: SessionId,
    pub run_id: Option<RunId>,
    pub call_id: ModelCallId,
    pub purpose: ModelCallPurpose,
    pub model: ModelSelection,
    pub input: ModelInputProjection,
    pub thinking_level: ThinkingLevel,
    pub stream_options: StreamOptions,
    pub max_output_tokens: Option<u64>,
}
```

这样 Driver 不再复制/重组 validated system/messages/tools/output contract。

#### 11. 推荐主流程

##### 新 user turn / work chain

```text
1. SessionRuntime admits PromptIntent.
2. capture TurnResourceSnapshot.
3. Tools.capture_profile_baseline().
4. prompt::assemble_turn() -> PromptTurn / PromptCallProfile.
5. PromptTurn.resolve_intent() -> canonical UserMessage.
6. bounded pre-commit transform/revalidation if enabled.
7. SessionWriter.commit(UserInput).
8. commit success -> publish invoked/message facts.
9. mark committed input EntryId as protected for pre-run compaction.
10. run pre-run threshold gate.
11. if compacted, rebuild SessionContext.
12. build final ConversationSeed containing current input exactly once.
13. allocate RunId, establish CurrentRun, publish run_started.
14. start Driver with ConversationSeed + DriverTurnInput + ToolBatchInvoker host.
```

current input protection 只存在于 Driver 启动前：

```text
commit U
  -> pre-run compaction excludes U by EntryId
  -> rebuild context includes U once
  -> ConversationSeed
```

Driver 启动后，U 是 live transcript 尾部的普通 user message，不再维护长期 `current_input` lane。

##### 每次模型调用

```text
Rig AgentRunStep::CallModel
  -> private rig adapter exposes current ordered transcript
  -> prompt::project_model_call(profile, conversation, transient overlay)
  -> ModelInputProjection<ModelMessage>
  -> ModelCallRequest { input: projection, model/options/purpose }
  -> SessionDriverHost.call_model()
  -> ModelGateway
  -> provider adapter
  -> normalized ModelCallResult
  -> Driver feeds result to Rig
```

如果 Rig API 必须区分 `{ history, prompt }`，该拆分仅存在于 `driver/rig.rs` 私有 adapter。MiniCore 外部 interface 只认 ordered `ConversationSeed`。

##### Tool round

```text
Rig CallTools
  -> DriverHost.invoke_tool_batch
  -> ToolBatchInvoker policy/approval/execution
  -> complete ToolRoundCandidate
  -> RunLink.commit_tool_round
  -> actor validates active run / abort ordering
  -> SessionWriter.commit(ToolRound)
  -> commit success returns exact committed assistant/tool-result delta
  -> publish committed message/tool facts
  -> Driver feeds finalized results to Rig
  -> Rig live transcript advances
  -> next CallModel
```

必须保证：

- commit 完成前 `invoke_tool_batch()` 不返回；
- commit 失败后不调用 `AgentRun::tool_results()`；
- abort-before-commit-admission 丢弃 candidate；
- commit 开始后 writer 获胜，成功 round 保留，但 actor 可以阻止下一 model call 并以 aborted 收尾。

当前 `CommittedSessionBatch` 只返回 entry IDs。为了让 Driver 使用 writer-finalized exact delta，建议扩展为返回 committed entries，或让 actor 用原 draft + returned IDs 生成不可再变的 `CommittedConversationDelta`：

```rust
pub struct CommittedConversationDelta {
    pub entries: Arc<[SessionEntry]>,
    pub leaf_id: EntryId,
}
```

##### Active Steer

```text
safe point
  -> actor consumes structured Steer intent
  -> active PromptTurn resolves canonical UserMessage
  -> commit UserInput
  -> commit success returns exact committed user delta
  -> append/rollover same RunId Rig segment
  -> next model call uses transcript ending in Steer message
```

不再返回语义含混的 `persistent_messages`；建议改名为 `committed_conversation_delta` 或 `rollover_input`。

##### FollowUp / NextTurn

- FollowUp 在当前 work chain 和必要 retry/recovery/pending action 完成后，创建新的 work chain、资源 capture、ConversationSeed 和新 `RunId`。
- NextTurn 继续只保存未展开 `PromptIntent`，由下一次显式 prompt boundary 统一解析；具体是合并为一个 Composite intent 还是按顺序 commit 多个 user messages，需要单独钉死，不应由 `Vec<MessageRecord>` 偶然决定。

##### Overflow recovery / compaction

```text
current run fails with ContextLimitExceeded
  -> run_finished(failed)
  -> Compaction phase
  -> compact committed session path
  -> preserve current work-chain required suffix by EntryId
  -> commit Compaction batch
  -> build_session_context()
  -> new ConversationSeed
  -> new RunId / DriveEntry::Continue mode
```

不 resume 旧 Rig serialized history。old Driver/Rig live transcript 在 run terminal 后丢弃；新 run 从 committed storage rebuild。

#### 12. Source-of-truth 与 owner 规则

推荐最终规则：

```text
SessionStorage owns what happened.
Prompt owns what the model sees.
Driver/Rig owns only the current run protocol projection.
ModelGateway owns provider invocation and encoding.
```

详细解释：

- `SessionStorage` / `SessionWriter`：唯一 durable truth。
- `ConversationSeed`：从 durable truth 构建的一次 run 初始只读投影，不可回写 storage。
- Rig history：run-scoped protocol working state，不是 durable truth。
- `PromptTurn`：pin stable resources/profile，并解析当前 intent；不拥有 history。
- `ModelInputProjection`：一次 call 唯一 model-visible truth。
- `ModelGateway`：只接收 provider-neutral model input，映射 provider DTO、auth、cache、fallback、usage/error。
- Provider cache/continuation：wire optimization，不反向成为 runtime history owner。

#### 13. 必须保留的现有设计

推荐方案不是把现有架构全部推倒。以下复杂度有真实不变量，应保留：

- `TurnResourceSnapshot`：work-chain stable resource inputs。
- immutable `PromptTurn` / `PromptCallProfile`：稳定 system prompt 和 tool schemas。
- `ToolProfileBaseline`：`ToolPromptView`、`ToolBatchInvoker`、fingerprint 同版。
- `SessionRuntime actor + RunTask + RunLink`：model/tool wait 时 mailbox 仍可响应。
- `SessionWriter` stable batches：UserInput、ToolRound、AssistantFinal、Compaction 原子提交。
- commit-before-next-call：工具副作用结果必须先稳定提交。
- `Driver` 作为 Rig sans-IO adapter。
- `ModelGateway` provider/auth/error/usage seam。
- compaction 不在 Driver/Rig 内执行，recovery 从 committed context 重建。

#### 14. 建议删除或修改的现有设计

| 当前设计 | 建议 |
| --- | --- |
| `ResolvedPromptInput { parts, messages }` | 改为 canonical single `UserMessage`；parts 降为 Prompt internal IR |
| `TurnState.messages` | 删除，或至少不再作为第二份 live history；run 初始历史进入 `ConversationSeed` |
| `DriveEntry::Prompt { messages }` | 删除消息容器职责；entry 只表示 start/continue/resume mode |
| 长期 `durable_history/current_input` lanes | current protection 收敛到 pre-run EntryId policy；Driver 只看 ordered conversation |
| `current_run_context/current_call_context` 两个 vectors | 合并成 single transient overlay，scope 置于 item |
| `ModelInputProjection.messages: MessageRecord[]` | 改为 `ModelMessage[]` |
| `ModelCallRequest` 再复制 projection fields | 改为持有 `ModelInputProjection` |
| `persistent_messages` | 改为 exact committed delta / rollover input |
| 多层重复 contribution stamps | 主要保留在 `PromptCallProfile` 与 `ModelInputProjection`；其它按需派生 |
| MVP future hook 完整 payload | 只保留 safe-point owner/禁止事项，字段形状延后 |

`TurnState` 如果继续存在，应只 pin resources/profile/model/tool baseline/limits/overflow budget，不再保存可与 Rig/storage 竞争的 history copy。

#### 15. 最终不变量

1. 一个 committed session fact 只有 `SessionStorage` 是 durable owner。
2. 一个 `PromptIntent` 在 MVP 解析为一条 canonical user message。
3. skill/template/resource 正文在 resolve 时展开；历史恢复不重新读取资源。
4. current input 在 pre-run compaction 中按 committed `EntryId` 保护。
5. `ConversationSeed` 包含 current input 恰好一次。
6. Driver/Rig 只能由 committed seed/delta 推进；未 commit draft 不进入下一 model call。
7. 完整 assistant tool-call + matching tool results 作为一个 `ToolRound` commit。
8. tool round commit 成功后才能调用 `AgentRun::tool_results()`。
9. active Steer commit 成功后才能 rollover/append。
10. Rig history 是 working state；commit failure、compaction 或 recovery 后不得作为新 run source。
11. `ModelInputProjection` 是唯一 model-visible projection；`ModelGateway` 不重新判断 session message visibility。
12. `ModelGateway` 只消费 `ModelMessage`，provider adapter 再生成 OpenAI/Anthropic/Gemini DTO。
13. system prompt 和 tool schemas 继续来自同一 `PromptCallProfile` / fingerprint。
14. transient context 不进入 storage，且不能插入 unresolved tool call/result 中间。
15. provider cache/continuation 必须与完整逻辑 input 等价；条件不满足时回退 full request。

#### 16. 实施顺序与 blocker

##### Step 1：Rig integration spike（首要 blocker）

必须验证 Rig 0.40.0：

- 如何用完整历史创建初始 `AgentRun`；
- 是否必须公开区分 history/prompt；
- `AgentRunStep::CallModel { prompt, history }` 的精确语义；
- tool call/result 后 `full_history()` 是否包含 MiniCore 需要的全部 provider-neutral内容；
- segment rollover 如何从 committed Steer 创建；
- 是否能把 ordered `ConversationSeed` 只在私有 adapter 内拆成 Rig 所需形状。

在完成 spike 前，不应接受删除 Prompt lanes 的 ADR。

##### Step 2：闭合 history seed

先引入 `ConversationSeed` / `InitialDriveContext`，保证 durable history 和 current input 能明确进入 Driver。即使最终仍采用保守 lane 方案，这一步也是 P0 必须修复。

##### Step 3：简化 current input

- `ResolvedPromptInput` 改为 canonical `UserMessage`。
- commit 后只保留 protected `EntryId` 用于 pre-run compaction。
- pre-run gate 后 seed 包含该消息一次。

##### Step 4：提交 delta

让 `UserInput`、`ToolRound`、Steer commit 返回或构造 exact committed conversation delta；Driver 只消费 committed delta。

##### Step 5：引入 `ModelMessage`

把 `MessageRecord -> ModelMessage` 的唯一转换放进 Prompt final projection；更新 `ModelInputProjection` 和 `ModelCallRequest`。

##### Step 6：收窄 call projection

将 `durable_history/current_input` 改为 ordered conversation + transient overlay；更新 compaction/current protection 文档。

##### Step 7：裁剪 future surface

删除 MVP 无 producer 的 context/hook/stamp 字段级契约；只保留明确 owner 和未来重新设计要求。

##### Step 8：新增 ADR 并同步权威文档

如果 spike 支持推荐方案，新增例如：

```text
ADR 0023: Driver Starts From One Committed Conversation Seed
```

需要同步：

- `CONTEXT.md`
- `docs/architecture.md`
- `docs/modules/README.md`
- `docs/modules/prompt.md`
- `docs/modules/session-runtime.md`
- `docs/modules/driver.md`
- `docs/modules/session-manager.md`
- `docs/modules/model-gateway.md`
- `docs/modules/compaction.md`
- `docs/modules/tools.md`
- ADR 0013、0017、0019、0021 的 amendment/supersede 关系
- Round 3 BR-055 及可能新增的 history-seed issue

#### 17. 测试建议

- 同一 `PromptIntent` 产生确定性 canonical user message/fingerprint。
- skill/template reload 后，已 committed message 不变化。
- pre-run compaction 不摘要当前 committed input。
- ConversationSeed 中 current input 只出现一次。
- 初始 Driver/Rig call 能看到完整历史 + current input。
- tool round writer 阻塞时，下一 model call 不发生。
- tool round commit error 后 Rig 不调用 `tool_results()`，run failed terminal。
- commit success 后下一 call 精确包含 committed assistant tool calls/results。
- active Steer commit 失败不进入 Rig；成功后同 `RunId` rollover 且仅出现一次。
- compaction/overflow recovery 丢弃旧 Rig transcript，从 storage seed 新 run。
- `MessageRecord::CompactionSummary` 在 Prompt projection 中稳定转成 model user summary。
- ModelGateway provider adapters 收到相同 `ModelMessage` 逻辑，分别映射 OpenAI/Anthropic/Gemini tool protocol。
- provider continuation prefix 条件不满足时回退 full request。
- transient context 不持久化、不切开 tool pair、不重复静态 resource。

#### 18. 当前暂停点与推荐下一步

当前没有修改权威架构文档，也没有关闭 BR-055。已形成的推荐是：

```text
优先做 Rig history-seed / rollover spike
  -> 若可行，采用 Transcript-First + ConversationSeed
  -> 若 Rig 强制暴露长期 history/prompt lanes，则采用保守 AgentRunSeed 方案
```

无论最终选择哪条路线，下列结论已足够确定：

- `ResolvedPromptInput.messages` 不是历史。
- current `parts/messages` 关系需要 canonical contract。
- history seed seam 当前缺失，属于 P0。
- `MessageRecord` 不应直接成为 provider-facing request message。
- committed session history 是 durable truth；Rig history 只能是 run-scoped working state。
- 当前消息流程应显著简化，不能原样进入生产实现。

### 2026-07-14 跨项目对照研究归档

日期：2026-07-14  
分支：`research/message-assembly-cross-project-study`  
完整报告（含综合对照 + pi / Codex / Claude Code 三份 subagent 原始调研附录）：

- [docs/research/message-assembly-cross-project-study.md](../research/message-assembly-cross-project-study.md)

同日在 BR-055 Transcript-First 推荐之上，用同一复杂场景（长历史已压缩 + 项目 md + skill 带图 + 工具轮 + mid-run steering）对照 pi、Codex、Claude Code，结论摘要：

- **三家共同收敛**验证目标方向：单一 live transcript；单一 session→provider 转换 seam；压缩摘要为 user 消息；steering 在工具轮边界注入；append-only 前缀稳定服务 prompt cache；**没有**贯穿整个 run 的 durable_history/current_input 双 lane。
- **建议 MVP 吸收**：压缩摘要请求复用会话前缀（Claude Code）；system prompt 确定性纪律与 fingerprint 未变时 `PromptCallProfile` Arc 跨 turn 复用；Codex 式 continuation 严格前缀判据留给 ModelGateway。
- **后期**：分级 snip/microcompact、compact 后 skill/文件恢复预算、skill 支持文件按需 Read。
- **维持**：项目 md 进 Profile 层（本地单用户）；commit-before-rollover 最严 steering；避免 pi 双生产路径。
- **阻塞未变**：Rig 0.40.0 spike → 通过后 ADR 0023 + 同步权威文档 + 关闭 BR-055。

本节仅作进度索引；细节、矩阵、场景六阶段流与附录全文以 research 文档为准。

## 2026-07-14 Accepted Decision Index: Transcript-First / ADR 0023

本节是对上方 BR-055 历史研究正文的接受决策索引；历史研究正文保留原貌，不倒改其当时的“推荐 / blocker / 暂停点”表述。最新权威决策见 [ADR 0023: Driver Starts From One Committed Conversation Seed](../adr/0023-driver-starts-from-one-committed-conversation-seed.md)。

- **状态**：Accepted；BR-055 在 round3 review 中改为 Resolved / Closed。
- **总名**：Transcript-First。
- **Rig spike 范围**：BR-049 / Rig 0.40.0 spike 只验证 `driver/rig.rs` 与 `model_gateway/rig.rs` 的 private adapter mapping；MiniCore 自有 public seam 不再等待 spike 反向决定。
- **Resource seam**：`ResourceManager.capture_turn_resources(...)`。
- **Tools seam**：`Tools.capture_turn_tools(...) -> TurnToolProfile`；`TurnToolProfile.prompt_view` 与 `TurnToolProfile.executor` 必须同 fingerprint。
- **Prompt seam**：`Prompt.prepare_message_turn(...) -> PreparedMessageTurn -> ModelContextProfile`；`compose_user_message(...) -> CanonicalUserMessage`；`assemble_model_context(...) -> AssembledModelContext`。
- **Conversation seam**：`ConversationSeed`、`CommittedConversationState`、`CommittedConversationDelta`。
- **Driver seam**：`Driver.drive_conversation(...)`，输入是 committed `ConversationSeed` + 窄 `DriverTurnInput`，不是完整 `TurnState`。
- **Model seam**：`ModelGateway.generate_model_turn(...)`；`ModelGateway` 只编码/调用 provider，不判断 session message visibility。
- **Commit seam names**：`execute_and_commit_tool_round`、`commit_pending_messages`、`commit_final_assistant_message`。
- **Compaction invariant**：Compaction 只做 cut/protection/directive；protected `EntryId` 不进入摘要目标；compaction commit 后先应用 committed delta 更新 `CommittedConversationState`，只有 leaf/revision mismatch 或 recovery 时才 reload storage，再构造 `ConversationSeed`。
- **Prompt ownership invariant**：Prompt 是 `AgentRun` 与 `CompactionSummary` 唯一模型上下文组装 seam；`AssembledModelContext` 是 model-visible truth。

## 2026-07-14 Handoff Snapshot: Transcript-First 文档落地完成

本节用于换机、上下文丢失或长时间中断后的快速恢复。它记录本轮已经提交的结果、当前仓库状态、不得重新打开的设计决定、仍待完成的工作以及建议恢复顺序。上方 BR-055 调研和跨项目研究保留的是决策过程；本节与 ADR 0023 记录当前结果。

### Git 状态与提交基线

- 工作分支：`research/message-assembly-cross-project-study`。
- 远端分支：`origin/research/message-assembly-cross-project-study`。
- 架构落地提交：`fe7f0d7 docs(architecture): adopt transcript-first message pipeline`。
- 该提交的直接研究父提交：`f65aa09 docs(research): archive message assembly cross-project study`。
- `fe7f0d7` 修改 25 个文档文件，新增 ADR 0023；本节所在的 progress 更新会作为后续独立提交记录。
- 当前仓库仍是纯文档设计仓库，没有 `Cargo.toml`、`src/` 或可执行测试；不要把伪代码误认为已实现接口。

换机后恢复命令：

```bash
git clone https://github.com/zqcli/minicore-runtime.git
cd minicore-runtime
git fetch origin
git switch research/message-assembly-cross-project-study
git pull --ff-only
git status --short --branch
git log -5 --oneline
```

### 本轮实际完成内容

- 新增 Accepted [ADR 0023](../adr/0023-driver-starts-from-one-committed-conversation-seed.md)，MiniCore public message pipeline 不再等待 Rig spike 反向定型。
- 更新 `CONTEXT.md`、`docs/architecture.md`、模块总览，以及 Prompt、SessionRuntime、Driver、SessionManager、ModelGateway、Compaction、Tools、ResourceManager、Skills、PromptTemplates、CommandSurface、Hooks、Events、Protocol、UsageStats 等权威文档。
- 为 ADR 0013、0017、0019、0021 增加 amendment，使旧 ADR 的局部接口草图明确服从 ADR 0023。
- 在 round3 review 中把 BR-055 改为 `Resolved / Closed`；旧 `ResolvedPromptInput.parts` 折叠问题由 `PromptIntent -> CanonicalUserMessage` 的单一 lowering seam 取代。
- 历史 research/review 正文没有倒改。其中 `PromptTurn`、`PromptCallProfile`、`DriveRequest`、`PromptProjection` 等旧名若出现在历史调研语境中，不代表当前权威接口；不要做全仓无差别机械重命名。

### 当前权威调用链

```text
RawSubmission
→ CommandSurface.parse_message_intent()
→ PromptIntent
→ ResourceManager.capture_turn_resources()
→ TurnResourceSnapshot.prompt_view()
→ Tools.capture_turn_tools()
→ TurnToolProfile { prompt_view, executor, fingerprint }
→ Prompt.prepare_message_turn(PromptResourceView + ToolPromptView)
→ PreparedMessageTurn
→ PreparedMessageTurn.compose_user_message(PromptIntent)
→ CanonicalUserMessage
→ SessionRuntime.commit_user_message()
→ CommittedConversationState.apply_committed_messages()
→ CommittedConversationState.build_conversation_seed()
→ Driver.drive_conversation(DriverTurnInput, ConversationSeed)
→ Prompt.assemble_model_context()
→ AssembledModelContext
→ ModelGateway.generate_model_turn()
```

工具轮和运行中输入的增量路径：

```text
ModelTurn(tool calls)
→ SessionDriverHost.execute_and_commit_tool_round()
→ TurnToolProfile.executor / ToolBatchInvoker
→ SessionWriter.commit(ToolRound)
→ CommittedConversationDelta
→ LiveConversation.apply_committed_messages()
→ 下一次 Prompt.assemble_model_context()

Steer
→ SessionDriverHost.commit_pending_messages()
→ SessionRuntime commit
→ CommittedConversationDelta
→ LiveConversation.apply_committed_messages()
→ 下一次 Prompt.assemble_model_context()

Final assistant candidate
→ SessionRuntime.commit_final_assistant_message()
→ commit/apply 成功
→ 才允许发布 completed terminal
```

### 不得回退的核心不变量

- `SessionStorage` 是 durable truth；`CommittedConversationState` 是只应用成功 commit 返回值的内存热投影，不是第二个 durable owner。
- session open/recovery 时从 storage 建立 committed projection；稳态 turn 直接从热投影构造 `ConversationSeed`，不允许每个 turn 重扫 JSONL。
- `ConversationSeed` 是一条有序 committed transcript，当前 canonical user message 恰好出现一次；不恢复长期 `durable_history/current_input/previous_input` 多 lane。
- Driver 的 `LiveConversation` 只能从 `ConversationSeed` 初始化，并且只能应用 `CommittedConversationDelta`；draft user/tool/steer message 不得提前对模型可见。
- user input、完整 ToolRound、Steer 和 final assistant 都遵循 commit-before-model-visible / commit-before-terminal。
- resources、tools 和 `PreparedMessageTurn` 只在新的 user turn / work chain 边界捕获一次；tool rounds、automatic retry、overflow recovery、active Steer 和同一公开 `RunId` 的 Rig segment rollover 复用该 captured profile。
- 同一 captured inputs 必须生成确定、稳定的 `ModelContextProfile`；conversation 本身会随成功提交的 ToolRound、Steer 和 assistant message 增长。
- Prompt 是 `AgentRun` 与 `CompactionSummary` 唯一回答“本次模型实际看见什么”的模块；不存在第二条 provider message assembly 路径。
- Prompt 只接收 `PromptResourceView` 与 `ToolPromptView`，不接收完整 `TurnResourceSnapshot`、`TurnToolProfile.executor`、Tools owner、storage 或 provider handle。
- `TurnToolProfile.prompt_view`、`executor` 和 profile 中记录的 tool fingerprint 必须一致；工具 schema/guideline 与真实执行器不得来自不同版本。
- `MessageRecord -> ModelMessage` 的唯一转换 owner 是 Prompt；`ModelGateway` 只接收 `ModelCallRequest { input: AssembledModelContext, ... }` 并负责 provider 编码、auth、调用、fallback、usage 与错误分类。
- MiniCore 逻辑上每次都构造完整 `AssembledModelContext`；provider continuation 只能由 adapter 在严格前缀等价时优化，不能改变逻辑 transcript。
- tool/message finished events 只能在完整稳定单元 commit 成功且 committed delta 已应用后发布；abort/failure 的 UI lifecycle close 语义必须与 durable commit 语义区分。

### Compaction 当前结论

- Compaction 只决定 cut point、protected `EntryId` 和 `CompactionSummaryDirective`，不拥有独立模型调用路径。
- 当前启动边界刚提交的 canonical user message 受 protected `EntryId` 保护，不进入同一边界的摘要目标。
- active/pre-run compaction 复用同一 work chain 的稳定 `ModelContextProfile` 和尽可能长的 conversation prefix；standalone compaction 通过 `prepare_message_turn()` 产生确定性 profile。
- Prompt 把 directive instruction 作为最后一条 typed user message，并使用 `OutputContract::NoToolCalls`；provider 无法保证禁用工具时必须返回 capability error，不能静默执行工具。
- compaction commit 成功后先应用 committed delta 或在必要时 reload committed projection，再构造新的 `ConversationSeed`；旧 run 的 transient overflow error、partial assistant 和未提交状态不进入重试上下文。
- context-limit source 统一为 `PromptAssembly | Provider`，两者共享一次有界 recovery policy，但 diagnostics 和 usage 来源保持区分。

### 验证与审查结果

- `git diff --check` 已通过。
- 当前权威文档的 Markdown 相对链接检查已通过，没有 broken links。
- 对 `CONTEXT.md`、`docs/architecture.md` 和 `docs/modules/` 做过旧 message-pipeline 术语扫描；剩余 `ActiveModelPromptProjection` / `ModelPromptProjection` 是 ResourceManager 的合法模型资源投影类型，不是旧 `project_model_call` seam。
- 两轮独立 reviewer 复核已完成。已修复完整 resource/tool profile 泄漏到 Prompt、ToolRound finished event 早于 commit、Compaction profile 策略冲突、`ModelCallPurpose` 重复、`MessageRecord` 泄漏到 gateway、`prepared_message_turn/system_profile/invoker` 命名残留，以及 `PromptAssembly` source 漂移。
- 最终 reviewer 没有 blocking finding；最后三处纯术语 warning 已随后修复。
- 因仓库没有实现代码，本轮没有代码测试；以上验证只能证明文档内部一致性，不能代替 Rig/provider integration test。

### 仍未完成与不要混淆的事项

- **BR-049 Deferred**：Rig 0.40.0 integration spike 仍未执行。它只验证 `driver/rig.rs` / `model_gateway/rig.rs` 私有映射：完整 history seed、Rig `{ history, prompt }` split、`full_history()`、`ModelTurn`、usage、tool names、`NeedsResolution`、`preresolved_result` 和 committed Steer segment rollover。
- **BR-050 Deferred**：MVP 不启用通用 `bash`；未来启用 external executor 前必须先完成 OS enforcement capability gate。
- **BR-051 / BR-052 Deferred**：进入任何生产纵切前必须做全仓 interface contract closure 与 MVP command payload/reachability review。重点是 `ToolBatchResult`、Driver errors/checkpoints/limits、sink 方法集、`UserInput`、`StreamOptions`、`ThinkingLevel` 和不可达的 `ResumeRun`。
- **仍 Open 的文档问题**：BR-056 retry attempt 跨 compaction 配对、BR-058 future protocol surface 裁边、BR-060 assistant-finished/usage event test rule、BR-061 post-MVP hook 与 MVP tool event 分界、BR-063 fingerprint/revision/token canonical algorithm、BR-064 Windows path 与 JSONL 单写入者、BR-065 skill 附件复现边界与命名双轨。
- **BR-059 Partially Resolved**：只剩旧 review/example 的 `open_handle/create_handle` 方法名机械统一；list request 形状已经定型，不应重开。
- historical review 中把 Rig spike 写成“ADR 0023 的前置 blocker”的段落是当时研究暂停点，已被本节和 ADR 0023 修正：public seam 已接受，spike 仅能验证或局部调整 private adapter。

### 建议下一步顺序

1. 先处理 BR-056、BR-058、BR-059、BR-060、BR-061、BR-063、BR-064、BR-065，减少进入实现前仍可机械关闭的文档歧义。
2. 联合执行 BR-051 / BR-052 contract closure review，为首批代码冻结 owner、字段、不变量、typed errors、cancel/partial semantics、protocol payload 和 command reachability。
3. 做隔离的 Rig 0.40.0 integration spike。spike 可以先于生产纵切验证 private mapping，但不得创建第二套 MiniCore public message/history seam。
4. spike 通过后精确 pin rig-core 版本并提交 lockfile，再按 ADR 0014 的 spine-first 顺序建立 `ModelGateway`、provider-neutral message 类型和 text-only Driver vertical slice。
5. 以 commit gate 建立最小 conformance matrix：UserInput、ToolRound、Steer、Compaction、AssistantFinal、abort/failure、overflow recovery、segment rollover、session reopen/recovery。
6. 首个纵切跑通后再扩展 tools/approval、dynamic context、provider continuation 和 post-MVP hooks；不要在 MVP spine 前实现完整 future surface。

### 换机后优先阅读顺序

1. 本节：当前 Git/状态/下一步。
2. [ADR 0023](../adr/0023-driver-starts-from-one-committed-conversation-seed.md)：接受决策和 public invariants。
3. [Architecture Message Pipeline](../architecture.md#message-pipeline)：端到端调用方向。
4. [Prompt](../modules/prompt.md)、[SessionRuntime](../modules/session-runtime.md)、[Driver](../modules/driver.md)：核心 owner 与运行时交互。
5. [SessionManager](../modules/session-manager.md)、[Tools](../modules/tools.md)、[ModelGateway](../modules/model-gateway.md)、[Compaction](../modules/compaction.md)：commit、execution、provider 与 recovery 细节。
6. [Round3 Review Issues](system-blueprint-review-issues-round3.md)：当前 Open/Deferred issue，不要只读上方旧研究暂停点。
7. [Cross-Project Study](../research/message-assembly-cross-project-study.md)：需要追溯 pi、Codex、Claude Code 对照依据时再读。
