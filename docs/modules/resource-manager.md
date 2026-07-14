# ResourceManager

`ResourceManager` 是 MiniCore 的内部资源子系统。它负责把会影响模型运行、在所属 scope 内稳定、且可冻结为 immutable projection 的 owner-produced 输入加载、分层、覆盖、诊断并发布为不可变 snapshot。它不是 UI 协议层，不执行 Agent turn，不执行工具，也不构造最终 system prompt。

资源的原始 owner 和 mutation owner 不因纳入 ResourceManager 而改变。`ResourceManager` 只冻结各 owner 产出的 prompt-safe projection；它绝不反向读取 `ModelState`、`Tools`、`AuthStore`、`SettingsStore`、provider client 或其他 owner handle。

## 设计定位

资源生命周期与 `AgentRuntime` / UI host 生命周期绑定。`OpenWorkspace` 初始化一次 `RuntimeResourceSnapshot`；MVP 没有 runtime reload、runtime-version drift、lazy cwd recompose 或 runtime current-pointer 替换语义。全局资源变化需要通过关闭并重建 `AgentRuntime` / host runtime 生效。

cwd 级资源可以显式 reload。成功的 cwd reload 构建新的 `CwdResourceSnapshot`，并通过 `ResourceSnapshotStore::replace_cwd(...)` 原子替换该 `(workspace_id, cwd)` 的 current pointer。`replace_cwd` 是 MVP 唯一的资源 current-pointer 替换线性化点。

```text
AgentRuntime / UI host lifecycle
  └─ OpenWorkspace
       └─ ResourceManager.ensure_runtime_snapshot_once(...)
            └─ RuntimeResourceSnapshot  // product/user-global defaults

OpenSession / NewSession / explicit cwd reload
  └─ ResourceManager.ensure_cwd_snapshot(...) / reload_cwd(...)
       └─ ResourceSnapshotStore.replace_cwd(...)
            └─ CwdResourceSnapshot      // project/cwd/trust/catalog overlay

SessionRuntime target user-turn boundary
  └─ ResourceManager.capture_turn_resources(...)
       └─ TurnResourceSnapshot          // turn behavior/model/environment/policy projections
            └─ Prompt.prepare_message_turn input
```

`ResourceManager` 是“资源如何加载、分层、覆盖、标识和发布”的单一事实来源；`SessionRuntime` 是“什么时候从各 owner 生成 typed projection、请求冻结并启动 turn”的 Pull Master；`Prompt` 负责解释 captured prompt-safe inputs、展开结构化 intent 和生成最终模型上下文。各 owner / `SessionRuntime` 只生成和校验 projection；`ResourceManager.capture_turn_resources(...)` 只绑定 current cwd snapshot、冻结这些值并计算 fingerprint。三者不能反向调用。

## Resource 定义

MiniCore 中的 `Resource` 定义为：

1. 对模型运行有影响；
2. 在所属 scope 内稳定，scope 可以是 runtime、cwd、turn 或后续 step；
3. 可冻结为 immutable projection；
4. projection 是 prompt-safe 或 tool-safe 的窄视图，不包含 owner 内部 handle、secret、executor、provider client、mutable state 或 raw settings；
5. projection 的来源、版本或 fingerprint 可用于 diagnostics 与可复现性。

这一定义允许 `TurnResourceSnapshot` 冻结 behavior/model/environment/policy 的模型可见 projection，但不把 `ModelState`、`Tools`、`AuthStore` 或 `SettingsStore` 的 owner 生命周期转交给 `ResourceManager`。例如：

- `ModelState` 仍由 `SessionRuntime` 拥有；turn capture 只接收 `ActiveModelPromptProjection` / model version。
- `Tools` 仍由 session-scoped `Tools` 子系统拥有；`TurnToolProfile.prompt_view` 传给 Prompt，`TurnToolProfile.executor` 只传给 run path，二者都不进入 `TurnResourceSnapshot`。
- provider/auth/settings 仍是 user-global/runtime-global service；ResourceManager 不读取 secret 或 settings owner handle。

## Snapshot 分层

资源 snapshot 分四层。MVP 实现 runtime、cwd、turn 三层，step 只保留类型与边界。

```text
RuntimeResourceSnapshot
  ↓ 被下层 pin 住
CwdResourceSnapshot
  ↓ 被下层 pin 住
TurnResourceSnapshot
  ↓ 作为父级
StepResourceSnapshot   // MVP reserved
```

`RuntimeResourceSnapshot` 和 `CwdResourceSnapshot` 是 `ResourceSnapshotStore` 中的 current snapshots。`TurnResourceSnapshot` 和 `StepResourceSnapshot` 不进入 `ResourceSnapshotStore`；它们只由 active turn / future step 持有。

### RuntimeResourceSnapshot

`RuntimeResourceSnapshot` 表示一次 `OpenWorkspace` 初始化得到的 product/user-global defaults。MVP 中它在 runtime 生命周期内不可替换、无递增 revision。

典型内容：

- builtin resources；
- user-global skills / prompt templates / context defaults；
- product default behavior、默认 prompt materials 和 user-global custom/append system prompt；
- source info、diagnostics、resource versions。

它不包含 cwd-local 项目资源，也不包含 provider/auth/model gateway 状态。provider settings、auth、custom providers 和 `ModelGateway` 是 user-global/runtime-global services，但不属于 resource snapshot。

### CwdResourceSnapshot

`CwdResourceSnapshot` 表示某个 `(workspace_id, cwd)` 下的一次 project/cwd/trust/catalog overlay 结果。它持有构建时使用的 runtime snapshot，并产出 cwd 下的 effective resource view。

```rust
pub struct CwdResourceSnapshot {
    pub workspace_id: WorkspaceId,
    pub cwd: PathBuf,
    pub revision: CwdResourceRevision,
    pub runtime: Arc<RuntimeResourceSnapshot>,
    pub local: CwdResourceLayer,
    pub resolved: ResolvedCwdResourceView,
    pub diagnostics: Vec<ResourceDiagnostic>,
}
```

`CwdResourceSnapshot` 不只是 cwd-local 增量。使用方应读 `resolved`，而不是临时合并 `runtime` 与 `local`。

`CwdResourceRevision` 只用于 cwd reload 与 UI 摘要失效；它不是 runtime 级版本，也不表达全局 drift。

### TurnResourceSnapshot

`TurnResourceSnapshot` 是一次 user turn / work chain 的稳定输入。它直接冻结本 turn 的非工具稳定 prompt 输入，并 pin 当前 cwd snapshot。

```rust
pub struct TurnResourceSnapshot {
    pub session_id: SessionId,
    pub user_turn_id: UserTurnId, // 明确不是 RunId
    pub cwd: Arc<CwdResourceSnapshot>,
    pub behavior: BehaviorProjection,
    pub model: ModelPromptProjection,
    pub environment: EnvironmentProjection,
    pub policy: PolicyProjection,
    pub fingerprint: TurnResourceFingerprint,
}
```

不再定义额外的 turn view wrapper 或 turn prompt snapshot 类型。如果没有 turn-local filtering，turn snapshot 仍是必要边界，因为它冻结了“本次 user turn 使用哪条 cwd snapshot 链，以及哪些 behavior/model/environment/policy projection”。

MVP `EnvironmentProjection` 只包含：

- workspace root；
- fixed session cwd；
- platform；
- date / time / timezone（来自可测试 clock）；
- interaction capabilities 的模型可见摘要。

MVP 不包含 VCS I/O、git status、文件扫描或其它需要 Prompt/ResourceManager 主动执行 I/O 的环境事实。后续 VCS 信息应由独立 context owner 或缓存 snapshot 以 typed context material 方式提供。

### StepResourceSnapshot

`StepResourceSnapshot` 为未来 safe-point / 每次模型采样前的动态资源预留。MVP 中只定义类型，不执行、不使用：

```rust
pub struct StepResourceSnapshot {
    pub parent: Arc<TurnResourceSnapshot>,
    pub step_id: StepId,
}
```

后续如果支持 turn 内 safe-point mutation（例如模型可见 profile 或工具批处理 executor 的合法替换），必须通过 `StepResourceSnapshot` 或明确 step override 记录，且在同一 actor transaction 中原子替换 `ModelContextProfile` 与 future tool executor，保持 fingerprint 一致。step capture 不能读取 `ResourceManager` current pointer，也不能重新加载 runtime/cwd 资源。

## Prompt 输入边界

`TurnResourceSnapshot` 是所有非工具稳定 Prompt 输入的 captured resource owner value。Prompt 不直接接收完整 snapshot，而是接收它投影出的窄 `PromptResourceView`；两者都不允许读取 ResourceManager current pointer。

```rust
impl TurnResourceSnapshot {
    pub fn materials(&self) -> PromptMaterials<'_>;
    pub fn behavior(&self) -> &BehaviorProjection;
    pub fn model(&self) -> &ModelPromptProjection;
    pub fn environment(&self) -> &EnvironmentProjection;
    pub fn policy(&self) -> &PolicyProjection;
    pub fn skill(&self, key: &ResourceKey) -> Option<&SkillResource>;
    pub fn template(&self, key: &ResourceKey) -> Option<&PromptTemplateResource>;
    pub fn fingerprint(&self) -> &TurnResourceFingerprint;
    pub fn prompt_view(&self) -> PromptResourceView;
}
```

`PromptResourceView` 只 pin `Arc<TurnResourceSnapshot>` 并暴露上述 prompt-safe getters；它不是第二个 snapshot layer、catalog owner 或可变缓存。`TurnResourceSnapshot` 复用本模块权威的 `ResourceKey`、`ResourceSourceInfo`、`ContentHash`、`CwdResourceRevision` 和 `TurnResourceFingerprint`。两者都没有 current pointer、reload、ensure、overlay、owner handle 或 filesystem I/O 能力。

工具提示素材保持独立，由 session-scoped `Tools` 产出 `TurnToolProfile`。Prompt 入口固定为：

```rust
pub fn prepare_message_turn(input: PrepareMessageTurnInput) -> Result<PreparedMessageTurn, PromptError>;

pub struct PrepareMessageTurnInput {
    pub resources: PromptResourceView,
    pub tools: ToolPromptView,
}
```

`TurnToolProfile` 不进入 `TurnResourceSnapshot`，也不整体进入 Prompt；`SessionRuntime` 只把其中的 `prompt_view` 放进 `PrepareMessageTurnInput`，把 `executor` 留给 `SessionDriverHost`。

## Capture 时机与 user-turn / work-chain 语义

`TurnResourceSnapshot` 按 user turn / work chain 捕获一次，而不是按每个 `RunId` 捕获：

- 新显式 user prompt、`FollowUp`、`NextTurn` 和新的 idle prompt：捕获新 `TurnResourceSnapshot`。
- automatic retry：复用原 `TurnResourceSnapshot`。
- context overflow compaction recovery：复用原 work chain 的 `TurnResourceSnapshot`。
- active `Steer`：使用 active `CurrentRun.prepared_turn` / active `TurnResourceSnapshot`。
- 同一 `RunId` 下 Rig segment rollover：复用原 `TurnResourceSnapshot`。

resource reload 只影响新的显式 user turn / work chain，不 patch active turn，也不改变已经展开并持久化的 skill/template invocation。

## 调用时机

```text
OpenWorkspace
  → ensure_runtime_snapshot_once()

OpenSession / NewSession
  → ensure_cwd_snapshot(workspace_id, cwd)

manual ReloadResources
  → reload_cwd(workspace_id, cwd)
  → ResourceSnapshotStore.replace_cwd(...)
  → AgentRuntime emits resources_changed summary

new explicit user turn / work chain
  → ResourceManager.capture_turn_resources(request, typed owner projections)
  → turn_tools = Tools.capture_turn_tools(...)
  → Prompt.prepare_message_turn(PrepareMessageTurnInput {
        resources: turn_resources.prompt_view(),
        tools: turn_tools.prompt_view.clone(),
     })
  → TurnState.prepared_turn / DriverTurnInput.context_profile
```

`capture_turn_resources(...)` 在稳态只读取 `ResourceSnapshotStore` 当前 cwd pointer，并冻结调用方传入的 typed projections。`OpenWorkspace`、`OpenSession` 和 `NewSession` 必须提前保证 runtime/cwd current snapshots 存在；capture 不是兜底读盘入口，也不是 lazy recompose 入口。

```rust
impl ResourceManager {
    fn capture_turn_resources(
        &self,
        req: TurnResourceCaptureRequest,
        projections: TurnResourceProjections,
    ) -> Result<Arc<TurnResourceSnapshot>, ResourceError> {
        let cwd = self
            .store
            .current_cwd(req.workspace_id, &req.cwd)
            .ok_or(ResourceError::MissingCwdSnapshot)?;

        Ok(Arc::new(TurnResourceSnapshot {
            session_id: req.session_id,
            user_turn_id: req.user_turn_id,
            cwd,
            behavior: projections.behavior,
            model: projections.model,
            environment: projections.environment,
            policy: projections.policy,
            fingerprint: TurnResourceFingerprint::from_parts(...),
        }))
    }
}
```

`capture_turn_resources` 是同步、无 I/O 的 actor 内操作；调用期间不能穿插会改变 behavior/model/environment/policy owner state 的 session command。它与 `replace_cwd` 按读取 current pointer 的时点线性化：

```text
resource capture 读取 current pointer 早于 replace_cwd(C2)
  → 合法使用旧 CwdResourceSnapshot

resource capture 读取 current pointer 晚于 replace_cwd(C2)
  → 必须使用 C2
```

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

## 资源更新

当前设计暂不考虑热更新、文件监听器或 snapshot 回调接口。MVP 资源更新只有两类：

1. `OpenWorkspace` 初始化 runtime snapshot；
2. `ReloadResources { workspace_id, cwd }` / session open ensure 构建或刷新 cwd snapshot。

```text
显式 cwd reload / session open ensure
  → ResourceManager 构建候选 CwdResourceSnapshot
  → ResourceSnapshotStore.replace_cwd(...) 原子发布
  → AgentRuntime 发出 resources_changed 摘要
```

旧 snapshot 永不原地修改。已经 running 的 turn 继续持有旧 `Arc<TurnResourceSnapshot>`；后续新的显式 user turn / work chain 捕获新的 current snapshot。

### 工作目录级资源 reload

```text
reload_cwd(workspace_id, cwd)
  → runtime = initialized RuntimeResourceSnapshot
  → 加载 cwd-local resources
  → overlay(runtime, local, ResourceOverlayPolicy)
  → 构建 CwdResourceSnapshot C2 { runtime, resolved }
  → ResourceSnapshotStore.replace_cwd(workspace_id, cwd, Arc<C2>)
```

reload 失败必须保留旧 current snapshot，并发布 diagnostics；不能让 `SessionRuntime` 看到半更新状态。

### ResourceSnapshotStore

```rust
pub trait ResourceSnapshotStore {
    fn runtime(&self) -> Arc<RuntimeResourceSnapshot>;

    fn current_cwd(
        &self,
        workspace_id: WorkspaceId,
        cwd: &Path,
    ) -> Option<Arc<CwdResourceSnapshot>>;

    fn replace_cwd(
        &self,
        workspace_id: WorkspaceId,
        cwd: PathBuf,
        snapshot: Arc<CwdResourceSnapshot>,
    );
}
```

`replace_cwd` 只替换当前指针，不修改旧 snapshot。旧 snapshot 的生命周期由 `Arc` 引用计数自然管理。runtime snapshot 在 MVP 中不暴露 replace API。

## 接口

```rust
pub trait ResourceManager {
    async fn ensure_runtime_snapshot_once(
        &self,
        reason: ResourceInitReason,
    ) -> Result<Arc<RuntimeResourceSnapshot>, ResourceError>;

    async fn ensure_cwd_snapshot(
        &self,
        request: CwdResourceRequest,
    ) -> Result<Arc<CwdResourceSnapshot>, ResourceError>;

    async fn reload_cwd(
        &self,
        request: CwdResourceRequest,
    ) -> Result<CwdResourceReloadResult, ResourceError>;

    fn capture_turn_resources(
        &self,
        request: TurnResourceCaptureRequest,
        projections: TurnResourceProjections,
    ) -> Result<Arc<TurnResourceSnapshot>, ResourceError>;
}

pub struct CwdResourceReloadResult {
    pub cwd: PathBuf,
    pub cwd_revision: CwdResourceRevision,
    pub changed: bool,
    pub summaries: ResourceSummaries,
    pub diagnostics: Vec<ResourceDiagnostic>,
}
```

`ResourceManager` 可以内部使用 `ResourceResolver`、`RuntimeResourceLoader`、`CwdResourceLoader`、`Skills`、`PromptTemplates` 等 helper，但这些 helper 不是架构 owner。

## 来源解析与信任

资源来源分层：

- builtin resources；
- user-global resources，例如 `agent_dir/skills`、`agent_dir/prompts`、user prompt files；
- trusted cwd/project resources，例如 `.minicore/skills`、`.minicore/prompts`、`AGENTS.md` / `CLAUDE.md` / `CONTEXT.md`；
- temporary explicit resources，例如 CLI/test/SDK 显式传入路径；
- 后续 extension/package resources。

所有 loaded resource 都应带 source info：

```rust
pub struct ResourceSourceInfo {
    pub path: Option<PathBuf>,
    pub source: String,
    pub scope: ResourceScope,
    pub origin: ResourceOrigin,
    pub base_dir: Option<PathBuf>,
}
```

项目级资源必须受 trust gate 保护。未信任 cwd 不加载项目级 prompt、skill、context 或 extension resource；user-global 和显式 temporary resources 可以继续可用。

trust state 是构建 `CwdResourceSnapshot.local` 的输入，不是 `ModelGateway` 或 provider/auth 的 scope。项目级 settings 不允许声明 custom provider、覆盖 provider base URL 或引用 credentials。

## Prompt materials、技能与模板

`ResourceManager` 只提供 prompt materials 和 selected resource catalog；最终 system prompt、结构化 intent 展开和 model context assembly 属于 [Prompt](prompt.md)。

```rust
pub struct PromptMaterials<'a> {
    pub custom_system_prompt: Option<&'a TextResource>,
    pub append_system_prompts: &'a [TextResource],
    pub context_files: &'a [ContextFile],
    pub skills: &'a SkillCatalog,
    pub templates: &'a PromptTemplateCatalog,
}
```

`PromptMaterials` 是 `CwdResourceSnapshot.resolved` 的只读投影，不是最终 system prompt 字符串。`ResourceManager` 保证这些素材和 selected skill/template body 来自同一版 captured snapshot；各 owner / `SessionRuntime` 在 turn boundary 生成 behavior/model/environment/policy projections，`ResourceManager.capture_turn_resources(...)` 将它们与 current cwd snapshot 一起冻结；`Prompt` 负责生成 immutable `PreparedMessageTurn` 和后续 `AssembledModelContext`。

显式 skill / prompt template invocation 的 delivery 时机属于 `SessionRuntime`，正文查询和消息组装属于目标 `PreparedMessageTurn.compose_user_message(...)`。为了保证旧 turn 可复现，snapshot 必须保存 selected body content，或保存 content hash + immutable loaded content reference；Prompt 不能让旧 turn 在运行中重新读取已被 reload 覆盖的文件内容。

## 静态资源与动态 Context

项目文件、ancestor context files、skills、prompt templates、自定义/追加 system prompt 和 future package/MCP static resources 必须先进入本模块的 trust/overlay/snapshot pipeline。RAG、memory、IDE diagnostics、实时 issue 状态等调用期结果可以由其 owner 作为 `ContextMaterialContribution` 进入 Prompt，但不能把已由 ResourceManager 管理的静态文件再读一遍并重复注入。

如果静态资源在 reload 时暂时不可用，ResourceManager 决定 reload 失败、diagnostic 或是否保持旧 current pointer；Prompt 只消费已经成功 captured 的 snapshot，不能自行回退到磁盘或另一个 revision。

## 事件与 RuntimeSnapshot

资源重载事件遵循 `resources_reload_started` → `resources_changed`。`ResourceManager` 不直接发布 UI event；`AgentRuntime` 调用它并发布协议事件。

`resources_changed` 只传摘要，不传技能全文、context file 正文或完整 system prompt。公开 `revision` 是这次 cwd effective resource view 的 `CwdResourceRevision`。

每个 `SessionSnapshot.resources` 表示该 loaded session 固定 cwd 的 current `CwdResourceSnapshot.resolved` 摘要；同一 cwd 的多个 loaded session 可以重复该摘要。完整 per-cwd catalog 属于 `ResourceManager` 内部状态，不额外放入 runtime-global snapshot 字段。adapter 需要未加载 cwd 或正文详情时走受控 query：

- `ResourceQuery::GetSkill { workspace_id, cwd, skill_name }`；
- `ResourceQuery::GetPromptTemplate { workspace_id, cwd, template_name }`；
- `ResourceQuery::GetContextFile { workspace_id, cwd, path }`，必须命中当前 snapshot 登记的 context file；
- `ResourceQuery::GetEffectivePrompt { session_id }`，debug/privileged query。

## 最小可行范围（MVP）

MVP 实现：

- `ResourceManager` 和 `ResourceSnapshotStore`；
- `RuntimeResourceSnapshot`、`CwdResourceSnapshot`、`TurnResourceSnapshot` 类型；
- `StepResourceSnapshot` 类型预留，不接入 run loop；
- user-global 和 trusted cwd resource directories；
- `ResourceOverlayPolicy`：cwd/project 覆盖 runtime/global，同 key 同层冲突有 diagnostics；
- skills catalog、prompt template catalog、context files、custom/append system prompt inputs；
- `OpenWorkspace` runtime 初始化、`OpenSession/NewSession` cwd ensure、显式 `ReloadResources` cwd reload；
- summary/detail 分离、source info、diagnostics、原子发布。

MVP 不实现：

- runtime/global resource reload；
- runtime current-pointer 替换；
- runtime 级版本 drift 检测；
- lazy cwd recomposition；
- 文件监听器、热更新或 snapshot 回调接口；
- step-level dynamic resource capture。

## 不应承担

`ResourceManager` 不应：

- 构造最终 system prompt；
- 展开 `/skill <name>`、兼容 `/skill:name` 或 prompt template 为 user message；
- 执行工具、审批工具或读取模型凭据；
- 拥有 session history 或 session persistence；
- 让 UI 直接读取本地资源文件；
- 接受 UI 提交的完整 resource snapshot 作为公开协议输入；
- 在 reload 中把部分成功结果暴露成半更新状态；
- 修改已经发布的旧 snapshot；
- 反向读取 `ModelState`、`Tools`、`AuthStore`、`SettingsStore` 或 provider owner handle。
