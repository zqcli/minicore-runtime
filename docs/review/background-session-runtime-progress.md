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
  - 每个 `SessionRuntime` / run pin 自己的 service generation；focused session 只影响 UI 和默认命令路由，不作为服务 scope 锚点。

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
- 将 `ResourceManager` 定义为运行时内部资源子系统，持有 `ResourceSnapshotStore`、内置 resolver、trust gate、diagnostics、reload/recompose pipeline 和 `ResourceOverlayPolicy`。
- 资源快照分层为 `RuntimeResourceSnapshot -> CwdResourceSnapshot -> TurnResourceSnapshot -> StepResourceSnapshot`；MVP 实现前三层，`StepResourceSnapshot` 只定义类型。
- `CwdResourceSnapshot` 持有构建时的 `Arc<RuntimeResourceSnapshot>`，并包含 cwd-local layer 与 overlay 后的 `resolved` effective view。cwd/project 资源可按 `ResourceKey { kind, namespace?, name }` 覆盖 runtime/global 同 key 资源。
- 每个 `SessionRuntime` 固定一个 workspace cwd；多个 session 可以共享同一个 cwd 的 current `CwdResourceSnapshot`。
- 每次 run 启动时通过 `ResourceManager.capture_turn(...)` 捕获 `TurnResourceSnapshot`，并用它构建 `TurnState`。
- `ReloadResources { workspace_id, cwd }` 为目标 cwd 构建新的 `CwdResourceSnapshot`；成功后原子替换 current pointer；running run 继续使用旧 `TurnState` / 旧 `TurnResourceSnapshot`，下一轮 user turn 使用新 snapshot。
- provider settings、auth、custom provider 和 `ModelGateway` 都是 user-global/runtime-global；项目级 settings 不允许声明 custom provider 或覆盖 auth/provider endpoint。
- 当前设计暂不考虑热更新、文件监听器或资源发现回调接口；资源更新走显式 reload / startup ensure。
- `ResourceManager` 和 `Prompt` 不直接互调；`SessionRuntime` 在 user turn 启动时捕获 `TurnResourceSnapshot`，从中提取 prompt materials，再合并 active tools / tool snippets / cwd / 日期等会话态，调用 `prompt::build_system_prompt(...)`。
- `Prompt` 是纯系统提示词构建模块，不读取文件、不读 `ResourceSnapshotStore`、不触发 reload；`ResourceManager` 不构造最终 system prompt，只发布结构化资源素材。
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
      │   │   ├─ (workspace_id, repo-a) -> CwdResourceSnapshot rev-a -> rev-r
      │   │   └─ (workspace_id, repo-b) -> CwdResourceSnapshot rev-b -> rev-r
      │   └─ ResourceOverlayPolicy
      ├─ user-global ProviderRegistry / AuthStore
      └─ shared ModelGateway

LoadedSessionRuntimes
  ├─ SessionRuntime A { cwd = repo-a } -> run captures TurnResourceSnapshot -> CwdSnapshot(repo-a, rev-a)
  ├─ SessionRuntime B { cwd = repo-a } -> next run captures current CwdSnapshot(repo-a)
  └─ SessionRuntime C { cwd = repo-b } -> run captures TurnResourceSnapshot -> CwdSnapshot(repo-b, rev-b)
```

## 已关闭问题

### BR-001

已关闭。`snapshot(session_id)` 已收敛为 `snapshot() -> RuntimeSnapshot`。

当前语义：

- `RuntimeSnapshot` 是 runtime 当前状态读模型。
- 不落盘。
- 不是 UI store。
- 不是 session index。
- 打开 workspace 后默认 `active_session = None`。
- `/resume` / GUI sidebar 通过 `ListSessions` 查询 `SessionIndex`。

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

最新修订：在当前产品约束下，resources 是唯一需要 cwd 维度快照化的运行输入；settings/auth/provider/model gateway 均为 user-global/runtime-global。`CwdScopedServices` 过重，已被 `ResourceManager` + 级联 immutable snapshots + run-time `TurnResourceSnapshot` capture 取代。多 session 隔离重点变为：session state 独立、session cwd 固定、`CwdResourceSnapshot.resolved` 提供 cwd effective resource view、run 启动时捕获 `TurnResourceSnapshot`、reload 只影响 future turn、工具副作用另由 tool policy/sandbox/mutation strategy 处理。

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

### 路线 A：当前已落文档的单进程 resource snapshot 模式

```text
AgentRuntime process
  └─ WorkspaceServices
      ├─ LoadedSessionRuntimes
      │   ├─ SessionRuntime A { cwd = repo-a }
      │   ├─ SessionRuntime B { cwd = repo-a }
      │   └─ SessionRuntime C { cwd = repo-b }
      ├─ ResourceManager
      │   └─ ResourceSnapshotStore
      │       ├─ current runtime -> RuntimeResourceSnapshot
      │       ├─ (workspace_id, repo-a) -> CwdResourceSnapshot
      │       └─ (workspace_id, repo-b) -> CwdResourceSnapshot
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

背景：pending tool approval 的恢复语义已落入正式文档：active session 当前 run 的待审批工具调用投影到 `RuntimeSnapshot.active_session.current_run.pending_tool_approvals`，`ToolApprovalBroker` 继续持有冻结的 `prepared_args`。下列内容已进一步收敛到 `docs/modules/tools.md`、`docs/modules/driver.md`、`docs/modules/session-runtime.md` 和 `docs/modules/agent-runtime-protocol.md`。

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

当前建议：统一采用 session-scoped `Tools` 子系统，由 `SessionRuntime` 协调 `DriverHost::invoke_tool_batch(...)` 与 `Tools::invoke_batch(...)`。`Tools` 内部封装 `ToolPolicy`、`ToolApprovalBroker`、RuntimeSnapshot projection、`ApprovalRequestId`、`ToolApprovalGrantStore` 和 `ToolExecutionCoordinator`，以吸收 Codex/pi request lifecycle 与并发调度经验。

## Tools 子系统文档落地

本轮已将口头设计正式落入模块文档：

- `Tools` 是 `SessionRuntime` 内部的 session-scoped 工具子系统，不是独立 runtime，也不是 UI 工具层。
- `SessionRuntime` 协调 `Driver` 和 `Tools`；`Driver` 只通过 `DriverHost::invoke_tool_batch(...)` 请求工具批量结果，不直接依赖 `Tools`。
- 代码层面确认：直接 `impl DriverHost for SessionRuntime` 是合法简化版；真实实现更推荐 per-run `SessionDriverHost` wrapper，因为它收窄访问面、承载 run-scoped context、避免 `self.driver + &mut self` 自借用压力，并让 `Driver` 更容易用 fake host 单测。
- `Tools::invoke_batch(...)` 是工具治理和执行的主入口，内部包含 registry、active tools、prompt catalog、policy、approval、grants、planner、coordinator、executors、sandbox 和 mutation queue。
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
- 典型 suspend checkpoint 包括：tool result 已产生但尚未回填 Rig / provider、等待用户交互、external job pending、用户在 safe point 主动暂停、host shutdown checkpoint。
- 普通 focus 切换不是暂停；pending approval 在 MVP 中是 current run 的 waiting substate，不等同于跨生命周期 suspend。

已同步完成：`agent-runtime-protocol.md`、`agent-runtime-events.md`、`driver.md`、`adr/0003-agent-runtime-events-use-event-msg-and-lifecycle-pairs.md` 和 `system-blueprint-review-issues.md` 已按该语义更新，BR-005 已标记为 Resolved。

## BR-008 Driver 输入 seam 收敛进展

本轮 grilling 已确认：BR-008 的核心不是 `DriverHost` / `SessionDriverHost` 是否存在，而是 `DriveRequest` 不应携带完整 `TurnState`。`DriverHost` 限制的是 driver 回调外部能力时能访问什么；`DriveRequest` 限制的是 driver 一开始被喂进来什么。只要 `DriveRequest { turn_state: TurnState }` 存在，`Driver` 就仍可见 `TurnResourceSnapshot`、resource revision、context usage 和工具治理状态，seam 仍然过宽。

设计收敛为：`SessionRuntime` 继续拥有完整 `TurnState`，run 启动时只投影 `DriverTurnInput` 放进 `DriveRequest`。`DriverTurnInput` 只包含 `model`、`system_prompt`、`active_tool_schemas`、`thinking_level` 和 `stream_options`；不包含 `TurnResourceSnapshot`、resource revision、context usage、queue/storage 或工具治理状态。turn resources 留在 per-run `SessionDriverHost` 中，用于构造 `ToolRunContext`、safe point 决策和未来 `StepResourceSnapshot` parent。

已同步完成：`driver.md`、`session-runtime.md`、`model-gateway.md`、`resource-manager.md`、`skills.md`、ADR 0011、ADR 0013、`CONTEXT.md` 和 `system-blueprint-review-issues.md` 已按该语义更新，BR-008 已标记为 Resolved。由于 `CONTEXT.md` 同时新增了 `TurnState` 词条，BR-017 也已标记为 Resolved。

## BR-009 ModelGateway 实现顺序收敛进展

本轮 grilling 已确认：BR-009 的风险来自阶段 5 需要真实 `DriverHost::call_model(...)`，但阶段 9 才准备完整 `ModelGateway`。如果阶段 5 为了跑通 text-only driver 写临时 provider/auth/usage/error 路径，阶段 9 会被迫拆掉重写。

设计收敛为：提前实现最小稳定 `ModelGateway` spine，而不是把阶段 9 的全部能力提前。阶段 4 改为 `Rig Driver + ModelGateway seam spike`，固定 `ModelSelection`、`ModelCallRequest`、`ModelCallResult`、`ModelCallErrorKind`、`ModelCallUsage` shape、`ModelGateway.call_model(...)`、最小 `ProviderRegistry.resolve(...)` 和 `AuthStore.resolve(...)`。阶段 5 的 text-only driver 只能走 `DriverHost::call_model -> ModelGateway.call_model(...)`，不允许 `Driver` / `SessionDriverHost` match provider、直读 env 或构造 Rig provider client。阶段 9 只在既有 spine 上扩展 custom provider、完整 auth、fallback、usage normalization 和 context usage。

已同步完成：`implementation-roadmap.md`、`model-gateway.md`、ADR 0014、`CONTEXT.md` 和 `system-blueprint-review-issues.md` 已按该语义更新，BR-009 已标记为 Resolved。
