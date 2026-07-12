# ResourceManager

`ResourceManager` 是 MiniCore 的内部资源子系统。它负责加载、组合、缓存和发布模型可见资源的不可变 snapshot；它不是 UI 协议层，不执行 Agent turn，不执行工具，也不构造最终 system prompt。

资源不是在每次使用时实时读文件，也不是回调修改旧 snapshot，而是在明确的加载 / reload 点生成新的 snapshot revision。运行中的 turn 只使用启动时捕获的 snapshot；reload 只影响后续 turn。Codex 的 turn/step context 和 pi coding-agent 的资源加载路径是设计参考对象，但不构成兼容承诺。

## 设计定位

`ResourceManager` 统一负责资源生命周期、来源解析、trust、overlay、diagnostics 和 immutable snapshot 发布；`SessionRuntime` 在 turn 启动时捕获资源，Prompt 只消费 captured `PromptResourceView`。MVP 预留 step snapshot 类型，但不启用 step 级资源刷新。

```text
AgentRuntime
  └─ WorkspaceServices
       └─ ResourceManager
            ├─ ResourceSnapshotStore
            │    ├─ current runtime -> Arc<RuntimeResourceSnapshot rev-r>
            │    └─ (workspace_id, cwd) -> Arc<CwdResourceSnapshot rev-c>
            ├─ ResourceResolver / loaders
            └─ ResourceOverlayPolicy

SessionRuntime
  ├─ 固定 { workspace_id, cwd }
  ├─ user turn 启动时捕获 TurnResourceSnapshot
  ├─ 从 captured snapshot 创建 PromptResourceView
  └─ 汇合 tool/model/agent/environment/policy views 调用 prompt::begin_turn(...)

Prompt
  ├─ PromptTurn.resolve_intent(...) 展开 skill / prompt template
  └─ prompt::project_model_call(profile + call-time lanes) 生成最终 ModelInputProjection
```

`ResourceManager` 是“资源如何加载、分层、覆盖、标识和发布”的单一事实来源；`SessionRuntime` 是“什么时候捕获并组装”的 Pull Master；`Prompt` 负责解释 captured resources、展开结构化 intent 和生成最终模型输入。三者不能反向调用。

## 资源 Snapshot 分层

资源 snapshot 分四层。MVP 实现前三层，第四层只定义类型和边界。

```text
RuntimeResourceSnapshot
  ↓ 被下层 pin 住
CwdResourceSnapshot
  ↓ 被下层 pin 住
TurnResourceSnapshot
  ↓ 作为父级
StepResourceSnapshot
```

### 运行时资源快照（RuntimeResourceSnapshot）

`RuntimeResourceSnapshot` 表示 UI/runtime 启动级或 runtime reload 后的 user-global / runtime-global 资源版本。

典型内容：

- builtin resources。
- user-global skills / prompt templates / context defaults。
- runtime/global extension/package resources，仅在后续 ADR 明确定义后加入。
- custom system prompt / append system prompt 的 user-global 输入。
- source info、diagnostics、resource revision。

它不包含 cwd-local 项目资源，也不包含 provider/auth/model gateway 状态。provider settings、auth、custom providers 和 `ModelGateway` 是 user-global/runtime-global services，但不属于 resource snapshot。

### 工作目录资源快照（CwdResourceSnapshot）

`CwdResourceSnapshot` 表示某个 `(workspace_id, cwd)` 下的一次资源解析结果。它必须持有构建它时使用的 runtime snapshot：

```rust
pub struct CwdResourceSnapshot {
    pub workspace_id: WorkspaceId,
    pub cwd: PathBuf,
    pub revision: ResourceRevision,
    pub runtime: Arc<RuntimeResourceSnapshot>,
    pub local: CwdResourceLayer,
    pub resolved: ResolvedCwdResourceView,
    pub diagnostics: Vec<ResourceDiagnostic>,
}
```

`CwdResourceSnapshot` 不只是 cwd-local 增量；它是这个 cwd 下的 effective resource view。使用方应读 `resolved`，而不是临时合并 `runtime` 与 `local`。

### 轮次资源快照（TurnResourceSnapshot）

`TurnResourceSnapshot` 是一次 user turn / run 的资源输入。它只 pin `Arc<CwdResourceSnapshot>`，不再单独 pin runtime snapshot：

```rust
pub struct TurnResourceSnapshot {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub cwd: Arc<CwdResourceSnapshot>,
    pub view: TurnResourceView,
}
```

`cwd.runtime` 已经 transitively pin 住 runtime revision，避免 turn 内出现 `runtime rev-r2` 与 `cwd rev-c1 built from rev-r1` 的 split-brain。

MVP 如果没有 turn-local resource filtering，`TurnResourceView` 可以为空或等同于 `cwd.resolved`；仍建议保留 wrapper，方便后续承载 session-local skill/tool visibility、turn diagnostics、rendered prompt inputs 和 step parent pointer。

### 提示词资源视图（PromptResourceView）

`PromptResourceView` 是从 captured `TurnResourceSnapshot` 产生的只读窄投影，不是新的 snapshot 层或 catalog owner：

```rust
pub struct PromptResourceView {
    snapshot: Arc<TurnResourceSnapshot>,
}

impl PromptResourceView {
    pub fn materials(&self) -> PromptMaterials<'_>;
    pub fn skill(&self, key: &ResourceKey) -> Option<&SkillResource>;
    pub fn template(&self, key: &ResourceKey) -> Option<&PromptTemplateResource>;
    pub fn revision(&self) -> ResourceRevision;
}
```

它必须复用本模块权威的 `ResourceKey`、`ResourceSourceInfo`、`ContentHash` 和 `ResourceRevision`，不能在 Prompt 侧再定义 `PromptResourceKey` / `PromptResourceRegistry`。`PromptResourceView` 只 pin 住 captured snapshot；它没有 current pointer、reload、ensure、recompose 或 overlay 能力。

### 步骤资源快照（StepResourceSnapshot）

`StepResourceSnapshot` 预留给未来每次模型采样前捕获动态资源：MCP runtime snapshot、environment/capability roots、dynamic tool inventory 或 world-state patch。

MVP 中定义类型但不执行、不使用：

```rust
pub struct StepResourceSnapshot {
    pub parent: Arc<TurnResourceSnapshot>,
    pub step_id: StepId,
}
```

MVP 的 prompt 构建、skill 展开和 tool registry 都只读取 `TurnResourceSnapshot`；后期 hook system 若读取资源摘要，也必须通过 captured snapshot。

后续如果启用 step 级 snapshot，它也不应该重新获取或重新合成 runtime/cwd/turn 级资源。`StepResourceSnapshot` 只持有 `Arc<TurnResourceSnapshot>` 作为 parent；向上访问 runtime/cwd 资源只是普通引用解链：

```text
step.parent.cwd.resolved.*
step.parent.cwd.runtime.*
```

因此 step 级 capture 的默认成本应接近一次 `Arc` clone 和一个小对象分配。它不能触发文件扫描、cwd overlay、runtime reload 或 prompt material 重建。只有未来引入真正 step-scoped 的动态资源时，`capture_step(...)` 才允许采样那一小部分动态输入，并且成本必须由该动态资源自己的缓存 / revision 控制。

## 调用时机

`ResourceManager` 的调用点按生命周期分层，而不是在每个 Rig step 中重新加载资源：

```text
workspace / runtime startup
  → ensure_runtime_snapshot()

open session / new session / first use of cwd
  → ensure_cwd_snapshot(workspace_id, cwd)

manual ReloadResources
  → reload_cwd(workspace_id, cwd)
  → atomic publish current CwdResourceSnapshot

user turn start
  → capture_turn(session_id, turn_id, workspace_id, cwd)
  → TurnState.resources = Arc<TurnResourceSnapshot>
  → TurnState.prompt_turn = prompt::begin_turn(PromptResourceView + typed session views)
  → SessionRuntime projects DriverTurnInput { model, prompt profile, options }

Rig AgentRun steps in the same turn
  → Driver uses DriverTurnInput only
  → SessionDriverHost / Tools read TurnState.resources when building ToolRunContext
  → do not call ResourceManager for model-visible static resources
```

也就是说，MVP 中 `Driver` / `AgentRunStep` 不直接调用 `ResourceManager`，也不直接接收 `TurnResourceSnapshot`。资源在 user turn 启动时被捕获进 `TurnState`，但跨到 `Driver` 的只有 `DriverTurnInput` 窄投影；需要资源/cwd 的工具运行上下文由 `SessionDriverHost` 从 `TurnState.resources` 构造。这样 reload、客户端 session selection 变化或其他 session 的资源变化不会污染当前 running turn，同时不把资源编排状态泄漏给 `Driver`。

`capture_turn(...)` 不负责发现文件变化，也不负责把磁盘上的新内容自动变成模型可见资源。MVP 中，模型可见资源只有在显式 `ReloadResources`、`reload_runtime()` 或首次 `ensure_*_snapshot()` 成功后，才会发布新的 current snapshot。下一次 user turn 会捕获当时的 current snapshot：如果此前没有 reload，捕获到的仍是同一个 revision；如果 reload 已成功发布新 snapshot，捕获到的就是新 revision。

因此 `reload` 是资源更新的写入边界，`capture_turn` 是资源使用的读取边界。保留 `capture_turn` 的必要性不在于自动刷新，而在于冻结一次 turn 的输入：同一个 running turn 内的 system prompt、skill 展开、prompt template 展开和 resource diagnostics 必须经同一个 `PromptResourceView` 来自同一版 snapshot，不能因为 turn 运行中发生 reload 或客户端 selection 变化而混用新旧资源。

不是所有层都在 turn 开始时才创建或冻结。`RuntimeResourceSnapshot` 和 `CwdResourceSnapshot` 可以在 workspace startup、session open、首次使用某 cwd 或显式 reload 时创建并发布；它们一经发布就是不可变对象。turn 开始时冻结的是 `TurnResourceSnapshot`：它读取当时 current `CwdResourceSnapshot`，而该 cwd snapshot 已经 pin 住它构建时使用的 runtime snapshot。也就是说，turn capture 冻结“本 turn 使用哪条 snapshot 链”，但不负责重新创建所有上层 snapshot。

打开 UI 后尚未发生任何 turn 时，第一次 turn 会拿到什么资源取决于 current snapshot 是否已经存在，以及是否发生过显式 reload：

```text
cwd snapshot 尚未 ensure
  → 第一次 turn 触发 ensure_cwd_snapshot()
  → 从当时磁盘状态加载资源

cwd snapshot 已在 open session / startup ensure 时创建为 rev-1
  → 之后只改磁盘文件但不 reload
  → 第一次 turn 仍捕获 rev-1

cwd snapshot 已创建为 rev-1
  → 第一次 turn 前执行 ReloadResources 并发布 rev-2
  → 第一次 turn 捕获 rev-2
```

例如 `skill-a` 在当前 `CwdResourceSnapshot rev-1` 中的正文是 v1。用户本地把 `SKILL.md` 改成 v2，但没有执行 `ReloadResources`，那么 current snapshot 仍是 rev-1；即使开始新的 user turn，显式调用 `skill-a` 时也必须从 captured `TurnResourceSnapshot -> CwdResourceSnapshot rev-1` 读取 v1。只有 reload 成功发布 `CwdResourceSnapshot rev-2` 后，后续 user turn 才会捕获并使用 v2。若 reload 发生在某个 turn 运行中，该 running turn 仍继续使用 v1，下一轮 turn 才使用 v2。

如果未来启用 step 级资源快照，调用点应是“下一次模型采样前”，通常对应 `AgentRunStep::CallModel` 之前，而不是每个 token delta、每个 UI event 或每个工具进度事件。step capture 也应以已经存在的 `Arc<TurnResourceSnapshot>` 为 parent，不能绕回 `ResourceManager.current_runtime()` / `current_cwd()` 去读取最新 current pointer，否则会破坏 running turn 的隔离语义。

## 覆盖策略

cwd 级资源可以覆盖 runtime/global 级资源。例如同名 skill 同时存在于 user-global 和 project cwd 时，cwd skill 应成为该 cwd 下的 selected skill。

覆盖规则由 `ResourceOverlayPolicy` 集中定义。MVP 可以直接编码在 Rust 代码中，不需要 settings 或配置文件；未来如果要调整 precedence，只改 policy 实现。

```rust
pub struct ResourceKey {
    pub kind: ResourceKind,
    pub namespace: Option<String>,
    pub name: String,
}

pub enum ResourceLayer {
    Builtin,
    UserGlobal,
    RuntimeExtension,
    Project,
    ProjectExtension,
    Temporary,
}

pub struct ResolvedResource<T> {
    pub selected: Arc<ResourceCandidate<T>>,
    pub shadowed: Vec<Arc<ResourceCandidate<T>>>,
}
```

MVP 优先级：

```text
Builtin < UserGlobal < RuntimeExtension < Project < ProjectExtension < Temporary
```

规则：

- 覆盖必须按 `ResourceKey` 判断，至少包含 `kind + name`；skill 与 prompt template 同名不互相覆盖。
- 高优先级 layer 覆盖低优先级 layer。
- 同一 key、同一 layer priority 的多个候选必须 deterministic：按声明顺序 / canonical path 稳定排序，并记录 conflict diagnostic。
- `CwdResourceSnapshot.resolved` 保存 selected + shadowed，UI 摘要可以展示 shadowing，但 prompt builder、skill invocation 和 command catalog 只能使用 selected。

例子：

```text
RuntimeResourceSnapshot R1:
  skill review -> ~/.minicore/skills/review/SKILL.md

Cwd local layer for repo-a:
  skill review -> repo-a/.minicore/skills/review/SKILL.md

CwdResourceSnapshot C7.resolved:
  skill review.selected -> repo-a/.minicore/skills/review/SKILL.md
  skill review.shadowed -> [~/.minicore/skills/review/SKILL.md]
```

不同 cwd 可以得到不同 effective view；同 cwd 的多个 session 可以共享同一个 immutable `CwdResourceSnapshot`。

## 边界决策

`ResourceManager` 和 [AgentRuntimeProtocol](agent-runtime-protocol.md) 不在同一层。

```text
AgentRuntimeProtocol
  拥有: UI command/query/event/snapshot schema、routing ids、query routing 与 response envelope
  不负责: 文件发现、skill 解析、project trust 判定、resource content 缓存

ResourceManager
  拥有: 资源来源解析、trust gate、snapshot 分层、overlay policy、diagnostics、reload/recompose
  不负责: 直接接受 UI command、发布 UI event、把协议结构作为存储模型暴露
```

公开协议可以暴露 `ReloadResources`、`resources_changed` 摘要、每个 loaded `SessionSnapshot.resources` 摘要，以及后续受控 detail query。公开协议不能接受完整 `ResourceSnapshot` 作为输入，也不能让 UI adapter 直接读本地资源文件。

## 资源更新

当前设计暂不考虑热更新、文件监听器或 snapshot 回调接口。所有会影响模型行为的资源更新都走显式 reload / recompose pipeline：

```text
显式 reload / startup ensure
  → ResourceManager 构建候选 snapshot
  → ResourceSnapshotStore 原子发布新的当前指针
  → AgentRuntime 发出 resources_changed 摘要
```

旧 snapshot 永不原地修改。已经 running 的 turn 继续持有旧 `Arc<TurnResourceSnapshot>`；后续 turn 捕获新的 current snapshot。

### 运行时级资源 reload

```text
reload_runtime()
  → 构建 RuntimeResourceSnapshot R2
  → 发布 current runtime = R2
  → 已存在的 CwdResourceSnapshot { runtime = R1 } 对 running turns 继续有效
```

cwd snapshot 与 runtime snapshot 的对齐推荐 lazy recompose：

```rust
fn capture_turn(session) -> Arc<TurnResourceSnapshot> {
    let runtime = resource_manager.current_runtime();
    let cwd = resource_manager.current_cwd(session.workspace_id, &session.cwd);

    let cwd = if cwd.runtime.revision != runtime.revision {
        resource_manager.recompose_cwd(session.workspace_id, &session.cwd, runtime)
    } else {
        cwd
    };

    Arc::new(TurnResourceSnapshot { cwd, .. })
}
```

### 工作目录级资源 reload

```text
reload_cwd(workspace_id, cwd)
  → runtime = current RuntimeResourceSnapshot
  → 加载 cwd-local resources
  → overlay(runtime, local, ResourceOverlayPolicy)
  → 构建 CwdResourceSnapshot C2 { runtime, resolved }
  → 发布 current_cwd[(workspace_id, cwd)] = C2
```

reload 失败必须保留旧 current snapshot，并发布 diagnostics；不能让 `SessionRuntime` 看到半更新状态。

### reload 后下一轮 turn 如何拿到新 snapshot

reload 成功后的关键不是把新 snapshot 推送到每个 `SessionRuntime`，而是在 `ResourceSnapshotStore` 中原子替换 current pointer。下一轮 user turn 启动时，`SessionRuntime` 必须重新调用 `ResourceManager.capture_turn(...)`，由它从 store 读取当前指针。

```text
UI / CommandSurface
  → AgentRuntime::dispatch(Command::ReloadResources { workspace_id, cwd })
  → AgentRuntime::handle_reload_resources(...)
  → ResourceManager::reload_cwd(CwdResourceRequest { workspace_id, cwd })
  → ResourceManager 构建 CwdResourceSnapshot C2
  → ResourceSnapshotStore::replace_cwd(workspace_id, cwd, Arc<C2>)
  → AgentRuntime 发出 resources_changed { revision: C2.revision }，workspace coordinate 在外层 Event

下一轮对话：

UI / CommandSurface
  → AgentRuntime::dispatch(Command::SubmitPrompt { session_id, ... })
  → SessionManager::get_loaded_runtime(session_id)
  → SessionRuntime::submit_prompt(...)
  → SessionRuntime::start_user_turn(...)
  → ResourceManager::capture_turn(TurnResourceCaptureRequest { session_id, turn_id, workspace_id, cwd })
  → ResourceSnapshotStore::current_cwd(workspace_id, cwd)
  → 返回 Arc<CwdResourceSnapshot C2>
  → 构建 TurnResourceSnapshot T2 { cwd: Arc<C2> }
  → TurnState.resources = Arc<T2>
  → prompt::begin_turn(T2.prompt_view() + typed session views)
  → TurnState.prompt_turn = Arc<PromptTurn>
  → project DriverTurnInput { prompt: PromptCallProfile, ... }
  → Driver::drive_run(DriveRequest { turn: DriverTurnInput, ... })
```

因此实现必须满足这些不变量：

- `SessionRuntime` 不跨 turn 缓存 `TurnResourceSnapshot`。
- 每个 user turn start 都必须调用 `ResourceManager.capture_turn(...)`。
- `capture_turn(...)` 只读取 `ResourceSnapshotStore` 当前已发布指针；它不读磁盘、不自动 reload。
- `replace_cwd(...)` 是 reload 的线性化点；它完成之后开始 capture 的 turn 必须看到新 current pointer。
- 旧 `Arc<CwdResourceSnapshot>` 不原地修改，已经 running 的 turn 继续使用旧对象。

并发边界：

```text
SubmitPrompt 在 replace_cwd 之前已经开始 capture
  → 合法地拿到旧 snapshot

SubmitPrompt 在 replace_cwd 之后才开始 capture
  → 必须拿到新 snapshot
```

`ResourceSnapshotStore` 可以用 `ArcSwap`，也可以用 `RwLock<HashMap<CwdKey, Arc<CwdResourceSnapshot>>>`。关键要求是 `replace_cwd` 原子替换当前指针，`current_cwd` 读取当前指针，旧对象只由 `Arc` 引用计数决定生命周期。

### 初始化与更新入口

各级 snapshot 的创建和更新由固定 lifecycle / command handler 调用，不由 UI event 自己更新，也不由 `resources_changed` 更新。`resources_changed` 只是 snapshot 已经发布后的通知。

第一道保证是打开 workspace 时初始化 runtime snapshot：

```rust
impl AgentRuntime {
    async fn open_workspace(&mut self, req: OpenWorkspaceRequest) -> Result<()> {
        let services = WorkspaceServices::new(req.workspace_id, req.agent_dir, req.settings).await?;

        services
            .resource_manager
            .ensure_runtime_snapshot(ResourceInitReason::WorkspaceOpen)
            .await?;

        self.workspace_services = Some(services);
        Ok(())
    }
}
```

MVP 协议没有 `WorkspaceEvent::Opened` / `workspace_opened`。`OpenWorkspace` 的命令接收由 `CommandAck` 表达，打开后的 workspace state 从 `RuntimeSnapshot` 读取；初始化过程中只有实际资源事实通过 `resources_*` 事件发布。内部 `RuntimeHooks::WorkspaceOpened` 即使后期实现，也不是同名公开事件。

第二道保证是打开 / 新建 session 时初始化该 session 固定 cwd 的 snapshot：

```rust
impl AgentRuntime {
    async fn open_session(&self, req: OpenSessionRequest) -> Result<()> {
        let handle = self.session_manager.open_handle(req.session_id).await?;
        let workspace_id = handle.workspace_id();
        let cwd = handle.cwd();

        self.services
            .resource_manager
            .ensure_cwd_snapshot(CwdResourceRequest {
                workspace_id,
                cwd: cwd.clone(),
                reason: CwdResourceReason::SessionOpen,
            })
            .await?;

        let runtime = SessionRuntime::new(SessionRuntimeInit {
            session_id: handle.session_id(),
            workspace_id,
            cwd,
            handle,
            services: self.services.clone(),
        });

        self.session_manager.insert_loaded(runtime);
        self.emit(EventMsg::SessionOpened { ... });
        Ok(())
    }
}
```

第三道保证是 turn 启动时兜底 ensure / recompose：

```rust
impl ResourceManager {
    async fn capture_turn(
        &self,
        req: TurnResourceCaptureRequest,
    ) -> Result<Arc<TurnResourceSnapshot>> {
        let runtime = self
            .ensure_runtime_snapshot(ResourceInitReason::TurnCapture)
            .await?;

        let cwd = match self.store.current_cwd(req.workspace_id, &req.cwd) {
            Some(cwd) if cwd.runtime.revision == runtime.revision => cwd,
            Some(_) => {
                self.recompose_cwd(CwdResourceRequest {
                    workspace_id: req.workspace_id,
                    cwd: req.cwd.clone(),
                    reason: CwdResourceReason::RuntimeRevisionChanged,
                }, runtime).await?
            }
            None => {
                self.ensure_cwd_snapshot(CwdResourceRequest {
                    workspace_id: req.workspace_id,
                    cwd: req.cwd.clone(),
                    reason: CwdResourceReason::TurnCaptureMissingCwd,
                }).await?
            }
        };

        Ok(Arc::new(TurnResourceSnapshot {
            session_id: req.session_id,
            turn_id: req.turn_id,
            cwd,
            view: TurnResourceView::default(),
        }))
    }
}
```

`ensure_runtime_snapshot()` 和 `ensure_cwd_snapshot()` 必须是幂等的。实现上应使用 runtime 级初始化锁和 `(workspace_id, cwd)` 级加载锁，并在拿锁后 double-check，避免并发 open session / first turn 重复构建 snapshot。`ensure_*` 只在缺失或 revision 不兼容时构建；已有可用 snapshot 时直接返回当前 `Arc`。

显式 reload 是唯一重新读取资源来源并替换 current pointer 的命令路径：

```rust
impl AgentRuntime {
    async fn handle_reload_resources(&self, workspace_id: WorkspaceId, cwd: PathBuf) -> Result<()> {
        self.emit_workspace(
            workspace_id,
            EventMsg::Resources(ResourcesEvent::ReloadStarted { cwd: cwd.clone() }),
        );

        let result = self.services.resource_manager.reload_cwd(CwdResourceRequest {
            workspace_id,
            cwd,
            reason: CwdResourceReason::ExplicitReload,
        }).await?;

        self.emit_workspace(
            workspace_id,
            EventMsg::Resources(ResourcesEvent::Changed {
                cwd: result.cwd.expect("cwd reload result"),
                revision: result.cwd_revision.expect("cwd reload revision"),
                skills: result.summaries.skills,
                prompt_templates: result.summaries.prompt_templates,
                context_files: result.summaries.context_files,
                system_prompt: result.summaries.system_prompt,
                append_system_prompts: result.summaries.append_system_prompts,
                diagnostics: result.diagnostics,
            }),
        );

        Ok(())
    }
}
```

runtime reload 后不原地更新已有 cwd snapshot。下一次 `capture_turn(...)` 发现 `cwd.runtime.revision != current_runtime.revision` 时，调用 `recompose_cwd(...)`：复用旧 cwd snapshot 的 `local` layer，与新的 runtime snapshot 重新 overlay，然后 `replace_cwd(...)` 发布新 cwd snapshot。`reload_cwd(...)` 才重新读取 cwd-local 文件；`recompose_cwd(...)` 不应因为 runtime revision 变化而顺便扫描项目文件。

## 资源快照存储（ResourceSnapshotStore）

`ResourceSnapshotStore` 是 `ResourceManager` 内部保存当前指针的存储：

```rust
pub trait ResourceSnapshotStore {
    fn current_runtime(&self) -> Arc<RuntimeResourceSnapshot>;
    fn replace_runtime(&self, snapshot: Arc<RuntimeResourceSnapshot>);

    fn current_cwd(&self, workspace_id: WorkspaceId, cwd: &Path) -> Option<Arc<CwdResourceSnapshot>>;
    fn replace_cwd(&self, workspace_id: WorkspaceId, cwd: PathBuf, snapshot: Arc<CwdResourceSnapshot>);
}
```

`replace_*` 只替换当前指针，不修改旧 snapshot。旧 snapshot 的生命周期由 `Arc` 引用计数自然管理。

## 接口

```rust
pub trait ResourceManager {
    async fn ensure_runtime_snapshot(&self, reason: ResourceInitReason) -> Result<Arc<RuntimeResourceSnapshot>, ResourceError>;
    async fn reload_runtime(&self, reason: ResourceReloadReason) -> Result<ResourceReloadResult, ResourceError>;

    async fn ensure_cwd_snapshot(&self, request: CwdResourceRequest) -> Result<Arc<CwdResourceSnapshot>, ResourceError>;
    async fn reload_cwd(&self, request: CwdResourceRequest) -> Result<ResourceReloadResult, ResourceError>;

    async fn capture_turn(&self, request: TurnResourceCaptureRequest) -> Result<Arc<TurnResourceSnapshot>, ResourceError>;
}

pub struct ResourceReloadResult {
    pub scope: ResourceScope,
    pub cwd: Option<PathBuf>,
    pub runtime_revision: ResourceRevision,
    pub cwd_revision: Option<ResourceRevision>,
    pub changed: bool,
    pub summaries: ResourceSummaries,
    pub diagnostics: Vec<ResourceDiagnostic>,
}
```

`ResourceManager` 可以内部使用 `ResourceResolver`、`RuntimeResourceLoader`、`CwdResourceLoader`、`Skills`、`PromptTemplates` 等 helper，但这些 helper 不是架构 owner。

## 来源解析

资源来源分层：

- builtin resources。
- user-global resources，例如 `agent_dir/skills`、`agent_dir/prompts`、user prompt files。
- trusted cwd/project resources，例如 `.minicore/skills`、`.minicore/prompts`、`AGENTS.md` / `CLAUDE.md` / `CONTEXT.md`。
- temporary explicit resources，例如 CLI/test/SDK 显式传入路径。
- 后续 extension/package resources。

所有 loaded resource 都应带 source info：

```rust
pub struct ResourceSourceInfo {
    pub path: Option<PathBuf>,
    pub source: String,
    pub scope: ResourceScope,      // builtin / user / project / temporary
    pub origin: ResourceOrigin,    // top_level / package / extension / builtin
    pub base_dir: Option<PathBuf>,
}
```

source info 用于 UI 展示、diagnostics、shadowing/conflict 报告和 trust 审计。

## 项目信任

项目级资源必须受 trust gate 保护。未信任 cwd 不加载项目级 prompt、skill、context 或 extension resource；user-global 和显式 temporary resources 可以继续可用。

trust state 是构建 `CwdResourceSnapshot.local` 的输入，不是 `ModelGateway` 或 provider/auth 的 scope。项目级 settings 不允许声明 custom provider、覆盖 provider base URL 或引用 credentials。

## 上下文文件

context files 是 system prompt 素材，不是 session entry。MVP 建议支持：

- user-global context defaults。
- cwd ancestor context files，例如 `AGENTS.md` / `CLAUDE.md` / `CONTEXT.md`，只在 project trusted 时加载 project-local 内容。

```rust
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
    pub source: ResourceSourceInfo,
}
```

context file content 进入 snapshot；reload 只影响后续 turn，不改写已经运行或已经持久化的消息。

## 提示词素材

`ResourceManager` 只提供 prompt materials 和 selected resource catalog；最终 system prompt、结构化 intent 展开和 model-input projection 属于 [Prompt](prompt.md)。

```rust
pub struct PromptMaterials<'a> {
    pub custom_system_prompt: Option<&'a TextResource>,
    pub append_system_prompts: &'a [TextResource],
    pub context_files: &'a [ContextFile],
    pub skills: &'a SkillCatalog,
}
```

`PromptMaterials` 是 `CwdResourceSnapshot.resolved` 的只读投影，不是最终 system prompt 字符串。`ResourceManager` 负责保证这些素材和 selected skill/template body 来自同一版 captured snapshot；`SessionRuntime` 负责汇合 `PromptResourceView`、tools、model、agent、environment 和 policy views；`Prompt` 负责生成 immutable `PromptTurn`。

调用方向必须保持单向：

```text
SessionRuntime
  → ResourceManager.capture_turn(...)
  → TurnResourceSnapshot.prompt_view()
  → prompt::begin_turn(TurnPromptInputs { resources, tools, model, ... })
```

不允许：

```text
Prompt
  → ResourceManager.current_cwd(...)

ResourceManager
  → prompt::begin_turn(...)
```

这样可以保证 running turn 的 system prompt、显式 skill/template 展开和 contribution fingerprint 只使用本 turn 已捕获的资源链，不会因为 reload 后的 current pointer 改变而混用新旧资源。

`PromptTemplates` 不默认进入每次 system prompt。它们是显式调用资源，只有目标 turn 的 `PromptTurn.resolve_intent(...)` 处理 `PromptTemplateInvocation` 时才展开为 user message；完整类型和语法见 [PromptTemplates](prompt-templates.md)。

## 技能

技能文件处理由平级 [Skills](skills.md) 模块提供；`ResourceManager` 只负责生命周期、分层和 overlay：

```text
ensure_runtime_snapshot()
  → resolve builtin / user-global skill roots
  → skills::load_skill_catalog(inputs)
  → 生成 ResourceCandidate<SkillResource>
  → 写入 RuntimeResourceSnapshot.skills

ensure_cwd_snapshot(workspace_id, cwd)
  → resolve trusted cwd/project skill roots
  → skills::load_skill_catalog(inputs)
  → 写入 CwdResourceSnapshot.local.skills
  → overlay(runtime.skills, local.skills, ResourceOverlayPolicy)
  → 发布 CwdResourceSnapshot { resolved.skills }

reload_cwd(workspace_id, cwd)
  → 重新读取目标 cwd 的 skill 文件
  → 重新 overlay current runtime + cwd local
  → replace_cwd(...)
```

`ResourceManager` 在 skill 层面拥有这些能力：

- 决定 skill source roots。
- 按 runtime/global 与 cwd/project 分层加载 skill candidates。
- 调用 `skills.rs` 解析、校验和格式化 summary 所需数据。
- 附加 `ResourceSourceInfo`、`ResourceKey { kind: Skill, ... }` 和 `content_hash`。
- 执行 `ResourceOverlayPolicy`，记录 selected / shadowed。
- 将 selected skill catalog 放入 `CwdResourceSnapshot.resolved.skills`。
- 为 loaded `SessionSnapshot.resources`、`resources_changed` 和 `CommandSurface` 提供 `SkillSummary`。

`ResourceManager` 不应：

- 自己解析 skill frontmatter。
- 自己拼 `<skill>` block。
- 解析 raw `/skill <name>` 或兼容 `/skill:name`。
- 构造 user message。
- 执行 Agent turn。

显式 `InvokeSkill` 的 delivery 时机属于 `SessionRuntime`，正文查询和 `<skill>` message 组装属于目标 `PromptTurn.resolve_intent(...)`。为了保证旧 turn 可复现，snapshot 必须保存 selected skill body content，或保存 content hash + immutable loaded content reference；Prompt 不能让旧 turn 在运行中重新读取已被 reload 覆盖的文件内容。

`ResourceManager` 提供技能 metadata 给 [CommandSurface](command-surface.md) 的 `resources.skills` dynamic provider，用于生成 `/skill <name>` 和兼容 `/skill:{name}` 的 command nodes；它不解析 raw user input，也不展开技能正文。

## 提示模板

prompt templates 是可显式调用资源，不默认进入 system prompt。`ResourceManager` 负责加载、source info、name conflict diagnostics、cwd-over-runtime overlay 和 immutable body；[PromptTemplates](prompt-templates.md) 提供解析/展开 helper；`PromptTurn.resolve_intent(...)` 查询 captured catalog、解析 required skills、替换参数并构造 `ResolvedPromptInput`。`SessionRuntime` 只负责 admission、delivery 和 run 编排。

## 静态资源与动态 Context

项目文件、ancestor context files、skills、prompt templates、自定义/追加 system prompt 和 future package/MCP static resources 必须先进入本模块的 trust/overlay/snapshot pipeline。RAG、memory、IDE diagnostics、实时 issue 状态等调用期结果可以由其 owner 作为 `ContextMaterialContribution` 进入 Prompt，但不能把已由 ResourceManager 管理的静态文件再读一遍并重复注入。

如果静态资源在 reload 时暂时不可用，ResourceManager 决定 reload 失败、diagnostic 或是否保持旧 current pointer；Prompt 只消费已经成功 captured 的 snapshot，不能自行回退到磁盘或另一个 revision。

## 扩展 / 包资源发现边界

当前设计不实现 extension/package resource discovery，也不预留资源发现回调 API。资源来源先由 `ResourceManager` 内置 resolver 管理。后续如果需要 extension/package 声明资源来源，应通过新的 ADR 重新定义 trust、source info、diagnostics、overlay 和 snapshot 发布边界；extension 仍不能直接提交完整 snapshot，也不能直接把 prompt 正文塞进 `SessionRuntime`。

## 事件与 RuntimeSnapshot

资源重载事件遵循 `resources_reload_started` → `resources_changed`。`ResourceManager` 不直接发布 UI event；`AgentRuntime` 调用它并发布协议事件。

`resources_changed` 只传摘要，不传技能全文、context file 正文或完整 system prompt：

```text
resources_changed {
    cwd: PathBuf,
    revision: ResourceRevision,
    skills: Vec<SkillSummary>,
    prompt_templates: Vec<PromptTemplateSummary>,
    context_files: Vec<ContextFileSummary>,
    system_prompt: Option<TextResourceSummary>,
    append_system_prompts: Vec<TextResourceSummary>,
    diagnostics: Vec<ResourceDiagnostic>,
}
```

对应 `workspace_id` 只存在于外层 `Event`。公开 `revision` 是这次 cwd effective resource view 的 `CwdResourceSnapshot.revision`；`runtime_revision` / `cwd_revision` 仍可保留在 `ResourceReloadResult` 内部，用于 ResourceManager 判断重组和失效，不重复暴露到 msg。

每个 `SessionSnapshot.resources` 表示该 loaded session 固定 cwd 的 current `CwdResourceSnapshot.resolved` 摘要；同一 cwd 的多个 loaded session 可以重复该摘要。完整 per-cwd catalog 属于 `ResourceManager` 内部状态，不额外放入 runtime-global snapshot 字段。adapter 需要未加载 cwd 或正文详情时走受控 query：

- `ResourceQuery::GetSkill { workspace_id, cwd, skill_name }`。
- `ResourceQuery::GetPromptTemplate { workspace_id, cwd, template_name }`。
- `ResourceQuery::GetContextFile { workspace_id, cwd, path }`，必须命中当前 snapshot 登记的 context file。
- `ResourceQuery::GetEffectivePrompt { session_id }`，debug/privileged query。

## 最小可行范围（MVP）

MVP 实现：

- `ResourceManager` 和 `ResourceSnapshotStore`。
- `RuntimeResourceSnapshot`、`CwdResourceSnapshot`、`TurnResourceSnapshot` 类型。
- `StepResourceSnapshot` 类型预留，不接入 run loop。
- user-global 和 trusted cwd resource directories。
- `ResourceOverlayPolicy`：cwd/project 覆盖 runtime/global，同 key 同层冲突有 diagnostics。
- skills catalog、prompt template catalog、context files、custom/append system prompt inputs。
- 显式 `ReloadResources` / startup ensure；不实现文件监听器、热更新或 snapshot 回调接口。
- summary/detail 分离、source info、diagnostics、原子发布。

后续再加：

- package manager resource sources。
- extension/package 资源声明能力；需要另起 ADR 定义，不沿用后期 `RuntimeHooks` 的通用 hook seam。
- theme catalog 或 UI-only resource plane。
- MCP resources/prompts bridge。
- step-level dynamic resource capture。

## 外部项目对照

- Codex 把 protocol crate 与 core runtime 分开，并在 turn/step 边界捕获稳定输入。MiniCore 的 `TurnResourceSnapshot` / `StepResourceSnapshot` 采用同样边界，但把更宽的 product resources 收敛在 `ResourceManager`。
- 参考实现表明资源生命周期与最终 prompt 组装应分离。MiniCore 通过 `ResourceManager -> PromptResourceView -> PromptTurn` 固定该 seam，并使用多层不可变 snapshot 保证 turn 隔离。
- Rig 的 `AgentBuilder` 接收 `preamble()`、`context()`、tools 和 dynamic context；`loaders` 只是文件读取工具。因此 MiniCore 需要在 Rig 之上保留产品级 `ResourceManager`。
- MCP 把 prompts 和 resources 建模成 server-managed catalog，并支持 list/get/read。MiniCore 的 summary/detail 分离可以与此对齐，但 MVP 不做实时 listChanged。

## 不应承担

`ResourceManager` 不应：

- 构造最终 system prompt。
- 展开 `/skill <name>`、兼容 `/skill:name` 或 prompt template 为 user message。
- 执行工具、审批工具或读取模型凭据。
- 拥有 session history 或 session persistence。
- 让 UI 直接读取本地资源文件。
- 接受 UI 提交的完整 resource snapshot 作为公开协议输入。
- 在 reload 中把部分成功结果暴露成半更新状态。
- 修改已经发布的旧 snapshot。
