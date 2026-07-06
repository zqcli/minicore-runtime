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

## 当前分支修订

分支：`design/runtime-resource-snapshot-mode`

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

修订后的模型：

```text
AgentRuntime
  └─ WorkspaceServices
      ├─ EventBus
      ├─ SessionManager / SessionIndex
      ├─ CommandSurfaceService
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
          └─ ToolSandboxRoot / ToolGateway inputs

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
  ├─ ToolGateway A
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

### 路线 A：当前已落文档的单进程方案 B

```text
AgentRuntime process
  ├─ LoadedSessionRuntimes
  │   ├─ SessionRuntime A
  │   ├─ SessionRuntime B
  │   └─ SessionRuntime C
  └─ CwdServiceRegistry
      ├─ CwdScopedServices(repo-a, gen-1)
      └─ CwdScopedServices(repo-b, gen-1)
```

优点：

- 实现较轻。
- 不需要 IPC。
- 适合嵌入式 runtime repo。
- 当前文档已经收敛到此方向。

缺点：

- 需要仔细避免共享状态污染。
- 需要 `CwdScopedServices` / generation pinning。
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
9. 当前已提交的 `WorkspaceServices` / `CwdScopedServices` 是否保留为单进程实现细节，还是改写为 future worker-local services？

## 当前建议结论

短期建议：以当前分支的单进程 resource snapshot 模式作为 MiniCore MVP 文档基线。

中期建议：先实现 `WorkspaceServices + ResourceSnapshotStore + LoadedSessionRuntimes` 的纵切，验证同一 UI 下多个 session 同时 running、reload 只影响 future turn、run 使用启动时捕获的 resource snapshot。

长期如果要模仿 Claude Code：让 `AgentRuntime` 演进为 supervisor，管理多个 `SessionWorker` process、worktree、event multiplexing、approval routing 和 attach/detach。届时需要重新处理 UI reconnect / RuntimeSnapshot 范围问题，并重新评估是否需要 worker-local services。
