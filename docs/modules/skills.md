# Skill 子系统架构设计

状态：当前权威架构（ADR 0130后，生产实现待启动）
日期：2026-07-31

## 目的

本文定义MiniCore Skill子系统的基础对象、所有权、渐进披露、按需加载、缓存、explicit reload和Prompt Injection关系。

本文以以下关系为基础：

```text
MiniCoreRuntime 初始化一个 Arc<SkillService>
Runtime 将该 Service 注入 loaded Session execution / TurnExecutionContext
Turn 领域对象不持有 Skill、SkillView、LoadedSkill 或 TurnSkills
SkillService从captured SkillResourceView与WorkspaceSkillContext构建不可变SkillView
TurnExecutionContext捕获view，并从entry captured bytes延迟解析Skill
Injection 层把已加载 Skill 转换为本轮 Prompt contribution
```

以下内容不在本设计范围内：

- BuiltIn、Runtime、User、Workspace、Agent、Session 等 Skill 层级；
- 不同 Skill 来源之间的优先级、覆盖和 namespace 规则；
- Skill 配置的持久化格式；
- Skill invocation 的 protocol method 和 Item 表达；
- 文件监听器、远程 Skill source 或插件协议的具体实现；
- manager、actor 或 storage 的最终划分。

## 决策摘要

本设计确定以下要点：

- `SkillService` 在 `MiniCoreRuntime` 启动时初始化；
- 一个 Runtime 内的 loaded Session execution 共享同一个 `Arc<SkillService>`；
- durable Session/SessionDefinition不持有Service handle，也不复制Skill definitions、view entries或正文；
- SkillService负责Skill发现、shared initialize/reload与Workspace load/reload时捕获source bytes、metadata解析、过滤、per-Turn SkillView构建、完整内容lazy parse、缓存和diagnostics；
- SkillResourceView是shared reload publication root；SkillView是从captured resource root与WorkspaceSkillContext构建的Turn-local immutable view。模型可见投影只包含轻量metadata，entry私有持有captured source bytes；
- SkillView的模型可见投影不包含完整Skill内容；完整Skill内容可以lazy parse，但只能来自entry已捕获的bytes；
- Skill 内容默认按需加载；
- Turn 对象不保存任何 Skill 字段；
- 哪一个 Turn 在执行期间需要哪个 Skill，由 Turn execution 决定；
- TurnExecutionContext使用捕获的SkillView及其context通过SkillService请求Skill；
- Input/Steer的async resolve只发生在TurnExecutionContext，PromptSet保持同步纯内存；
- `SkillInjector` 只负责把已经加载的 Skill 转换成 Prompt contribution；
- Prompt 负责把 Skill contribution 与其他输入组装成最终模型上下文；
- 已加载 Skill 内容是不可变值；
- cache eviction或reload不会修改已经返回的`Arc<LoadedSkill>`；
- shared reload成功后原子替换current SkillResourceView，Workspace reload成功后替换Snapshot中的captured sources；两者都只影响future Turn，已加载内容和已经开始的模型调用不回写。

## 对象关系

```text
MiniCoreRuntime
├─ Arc<SkillService>
│  ├─ SharedSkillSourceAdapter*
│  ├─ WorkspaceSkillSourceAdapter*
│  ├─ derived SkillView cache
│  ├─ SkillContentCache
│  ├─ SkillLoadStateStore
│  └─ SkillDiagnostics
└─ SharedResourceRoots.skills: Arc<SkillResourceView>

loaded Session execution
└─ Runtime 注入 Arc<SkillService>

TurnExecutionContext
├─ Arc<SkillService>
├─ Arc<SkillViewContext>
├─ Arc<SkillView>
└─ SkillService::load(&view, &entry)
      └─ Arc<LoadedSkill>
         └─ SkillInjector
            └─ PromptContribution
               └─ live input normalization
```

## MiniCoreRuntime

`MiniCoreRuntime` 是 SkillService 的创建者和生命周期 owner：

```rust
pub struct MiniCoreRuntime {
    prompt_service: Arc<PromptService>,
    tool_service: Arc<ToolService>,
    skill_service: Arc<SkillService>,
    model_gateway: Arc<ModelGateway>,
    shared_resources: RwLock<SharedResourceRoots>,
}
```

Runtime 启动时：

1. 创建 Skill source adapters；
2. 创建 `SkillService`；
3. `SkillService::initialize()`返回initial SkillResourceView，由Runtime放入SharedResourceRoots；
4. 将同一个 `Arc<SkillService>` 注入后续 loaded Session execution / Turn capture；
5. Runtime shutdown 时停止新的 discovery/load，并释放子系统资源。

## Session

Durable Session和SessionDefinition不保存SkillService、SkillView或正文：

```text
Session / SessionDefinition
≠ Arc<SkillService>
≠ SkillView
≠ LoadedSkill
≠ Skill cache
```

Runtime在load/capture时把同一个SkillService注入Session execution。Turn admission使用exact AgentRevisionRef、SessionDefinitionRevision和本Turn pin的WorkspaceSkillContext构造SkillViewContext；SkillService不能从SessionId回查current SessionDefinition或自行扩大Workspace Skill source。

完整 Agent/Session lifecycle 见 [Agent 与 Session 生命周期架构设计](agent-session-lifecycle.md)，Workspace 授权见 [Workspace 子系统架构设计](workspace.md)。

## Turn

Turn 对象不持有 Skill：

```rust
pub struct Turn {
    pub id: TurnId,
    pub session_id: SessionId,
    pub started_at: Timestamp,
    pub status: TurnStatus,
}
```

Turn 不包含：

```text
skills
TurnSkills
SkillView
SkillEntry
SkillId[]
LoadedSkill[]
```

Turn执行期间若确定需要某个Skill，TurnExecutionContext使用本Turn捕获的SkillView entry和`WorkspaceSkillContext`请求完整内容。加载结果进入Injection和输入规范化流程，不写回Turn对象。load不查询reload后的shared root或future SkillView，不按path重新读取current filesystem，也不能把SkillInjection作为未提交的current-call Prompt旁路。完整capture、input composition和reload规则见[Turn执行模块与执行上下文架构设计](turn-execution-context.md)。

## SkillService

`SkillService`是Skill子系统的深模块，对外隐藏discovery、文件读取、解析、过滤、cache、并发去重、reload和diagnostics：

```rust
pub struct SkillService {
    shared_sources: Vec<Arc<dyn SharedSkillSourceAdapter>>,
    workspace_sources: Vec<Arc<dyn WorkspaceSkillSourceAdapter>>,
    views: SkillViewCache,
    content_cache: SkillContentCache,
    load_states: SkillLoadStateStore,
    diagnostics: SkillDiagnostics,
}

pub(crate) struct SkillResourceView {
    shared_sources: Arc<[CapturedSkillSource]>,
}
```

SkillView context固定当前Agent、Session和Turn-pinned Workspace Skill view：

```rust
pub struct SkillViewContext {
    pub agent: AgentRevisionRef,
    pub session_id: SessionId,
    pub session_revision: SessionDefinitionRevision,
    pub workspace: WorkspaceSkillContext,
}

```

基础 interface：

```rust
impl SkillService {
    pub(crate) async fn initialize(&self) -> Result<Arc<SkillResourceView>, SkillError>;

    pub(crate) async fn build_reload_candidate(
        &self,
    ) -> Result<Arc<SkillResourceView>, SkillError>;

    pub(crate) async fn capture_workspace_sources(
        &self,
        context: WorkspaceSkillCaptureContext,
    ) -> Result<Arc<[CapturedWorkspaceSkillSource]>, SkillError>;

    pub(crate) async fn for_turn(
        &self,
        resources: Arc<SkillResourceView>,
        context: Arc<SkillViewContext>,
    ) -> Result<Arc<SkillView>, SkillError>;

    pub(crate) async fn load(
        &self,
        view: &SkillView,
        entry: &SkillEntry,
    ) -> Result<Arc<LoadedSkill>, SkillLoadError>;

}
```

`initialize()`构建并返回第一个SkillResourceView。`build_reload_candidate()`读取并捕获required shared filesystem source bytes、构建candidate并validate，但不发布。SkillService不保存current pointer，也没有publish方法；Runtime把candidate放入完整`SharedResourceRoots`后一次publication。`for_turn()`使用Turn admission捕获的shared resource root与`WorkspaceSkillContext`中的captured Workspace sources构建immutable `SkillView`。watcher最多标记dirty，不自动publication；`load()`只允许从entry captured bytes lazy parse完整正文。

`capture_workspace_sources()`只接受Workspace candidate私有投影出的`WorkspaceSkillCaptureContext`，只能在其中authorized Skill roots内discover/read并返回immutable captured values。它不发布SkillView或WorkspaceSnapshot；Session lifecycle负责在Workspace resolve、Prompt capture和Skill capture全部成功后一次发布new Snapshot。

`for_turn()`把exact `Arc<SkillViewContext>`私有绑定进返回的SkillView。`load()`必须：

- 使用Turn捕获的`SkillEntry.location`定位source；
- 使用传入SkillView绑定的exact `SkillViewContext`；
- 在lazy parse前校验entry确实属于传入的captured SkillView，且source authorization/provenance一致；
- 不查询reload后的shared root或future SkillView；
- 不按path重新读取current file。

本设计采用显式reload一致性：文件变化只产生dirty diagnostic；尚未加载的entry也只能解析已捕获bytes，不能读到新正文。已经返回的`Arc<LoadedSkill>`和已经committed的UserMessage不被修改。

## Skill Source Adapter

不同物理来源通过内部 adapter 接入 SkillService：

```rust
pub trait SharedSkillSourceAdapter: Send + Sync {
    async fn discover(
        &self,
    ) -> Result<Vec<DiscoveredSkill>, SkillSourceError>;

    async fn capture(
        &self,
    ) -> Result<Vec<CapturedSkillSource>, SkillSourceError>;
}

pub(crate) trait WorkspaceSkillSourceAdapter: Send + Sync {
    async fn capture(
        &self,
        context: &WorkspaceSkillCaptureContext,
    ) -> Result<Vec<CapturedWorkspaceSkillSource>, SkillSourceError>;
}

pub struct CapturedSkillSource {
    location: SkillLocation,
    bytes: Arc<[u8]>,
    authorization: SkillSourceAuthorization,
    modified_at: Option<Timestamp>,
}

pub enum SkillLocation {
    Shared(PathBuf),
    Workspace(WorkspaceRelativePath),
}

pub enum SkillSourceAuthorization {
    Shared,
    Workspace(WorkspaceSourceAuthorization),
}

pub struct SkillSourceRef {
    location: SkillLocation,
    authorization: SkillSourceAuthorization,
}

impl SkillSourceRef {
    pub fn location(&self) -> &SkillLocation;
    pub fn authorization(&self) -> &SkillSourceAuthorization;
}

impl CapturedSkillSource {
    pub fn shared(
        location: SkillLocation,
        bytes: Arc<[u8]>,
        modified_at: Option<Timestamp>,
    ) -> Self;

    pub(crate) fn from_workspace(
        source: &CapturedWorkspaceSkillSource,
    ) -> Self;
}
```

Workspace Skill source adapter只能使用`WorkspaceSkillCaptureContext`中的authorized source roots，并必须通过context `capture(...)`重新验证location/authorization。Shared BuiltIn/User adapter不接收Workspace roots，也不能伪造Workspace authorization/provenance。Session lifecycle operation负责cancellation与publication前的current authority/revision validation；Skill adapter不拥有TurnControl。Workspace definition update只在Session Idle。本设计不定义BuiltIn/User/Workspace/Agent/Session等来源层级和优先级。

Workspace adapter必须通过`WorkspaceSkillCaptureContext::capture(...)`返回`CapturedWorkspaceSkillSource`。`SkillService::for_turn()`再使用`CapturedSkillSource::from_workspace(...)`转换为SkillEntry统一持有的captured type；转换把relative location映射为`SkillLocation::Workspace`，并clone immutable bytes和exact WorkspaceSourceAuthorization，不重新读取filesystem。

```rust
pub struct DiscoveredSkill {
    pub location: SkillLocation,
    pub modified_at: Option<Timestamp>,
    pub authorization: SkillSourceAuthorization,
}
```

## Skill View

SkillView是渐进披露的第一阶段：模型可见部分只包含轻量metadata，entry私有持有captured source bytes：

```rust
pub struct SkillView {
    context: Arc<SkillViewContext>,
    entries: Arc<[SkillEntry]>,
}

pub struct SkillEntry {
    pub metadata: SkillMetadata,
    pub location: SkillLocation,
    captured: Arc<CapturedSkillSource>,
}
pub struct SkillMetadata {
    pub id: SkillId,
    pub name: SkillName,
    pub description: String,
    pub path: PathBuf,
    pub scope: SkillScope,
}

impl SkillView {
    pub(crate) fn context(&self) -> &Arc<SkillViewContext>;
    pub fn entries(&self) -> &[SkillEntry];
}
```

`SkillView::entries()`返回不可变entry；crate-private `context()`返回创建该view的exact context Arc。Turn捕获`Arc<SkillView>`后不再查询shared current root或构建future view；模型可见projection只暴露必要metadata，省略location、authorization和captured bytes。

`SkillScope` 只作为 metadata 和 filtering 输入，不定义层级、覆盖或优先级语义：

```rust
pub struct SkillScope;
```

SkillView必须满足：

- 模型可见投影不包含Skill正文；
- entry私有持有shared publication或WorkspaceSnapshot中捕获的immutable source bytes；
- view一致性由同一个immutable `Arc<SkillView>` ownership和private construction保证；
- metadata顺序确定；
- `SkillId`稳定；
- `SkillName`冲突产生明确diagnostics；
- shared reload可以重建candidate SkillResourceView；future `for_turn()`从new shared root构建new SkillView；
- UI可见view与模型可见view可以使用不同安全投影。

模型披露建议只暴露必要字段，避免自动泄漏本地绝对路径：

```rust
pub struct ModelVisibleSkillMetadata {
    pub id: SkillId,
    pub name: SkillName,
    pub description: String,
}

pub struct SkillPromptView {
    entries: Arc<[ModelVisibleSkillMetadata]>,
}

impl SkillView {
    pub fn prompt_view(&self) -> SkillPromptView;
}

impl SkillPromptView {
    pub fn entries(&self) -> &[ModelVisibleSkillMetadata];
}
```

`SkillPromptView`字段和constructor保持private，只包含稳定排序后的模型安全metadata，并只能由parent `SkillView`私有投影。

## 完整 Skill 内容

完整内容只在 `load()` 后产生：

```rust
pub struct LoadedSkill {
    metadata: SkillMetadata,
    source: SkillSourceRef,
    content: Arc<SkillContent>,
}

pub struct SkillContent {
    body: String,
}

impl LoadedSkill {
    pub fn metadata(&self) -> &SkillMetadata;
    pub fn source(&self) -> &SkillSourceRef;
    pub fn content(&self) -> &SkillContent;
}

impl SkillContent {
    pub fn body(&self) -> &str;
}
```

## 加载状态

SkillService 单独维护实际加载状态：

```rust
pub enum SkillLoadState {
    Unloaded,
    Loading,
    Loaded,
    Failed {
        message: String,
    },
}
```

加载状态不进入 Session、Turn、SkillMetadata 或 SkillContent。

基础规则：

- SkillView metadata可用不代表正文已经加载；
- 同一内部cache key的并发load应合并为一次底层读取和解析；
- content cache保存不可变`Arc<SkillContent>`，不缓存带某个Session authorization/provenance的完整LoadedSkill；
- load失败进入diagnostics，并允许后续重试；
- 默认baseline是metadata discovery + content lazy load；
- 不引入Eager/Lazy/Manual多级LoadPolicy。

## Cache

内容cache使用内部文件状态作为key：

```text
SkillCacheKey {
    location,
    captured_bytes,
    parser_version,
}
```

cache 规则：

- 同一个 key 返回同一个或等价的不可变 `Arc<SkillContent>`；
- content cache不保存process-local cancellation/security signal；
- 每次`load()`都校验entry属于传入SkillView且source authorization/provenance一致，再从captured bytes解析或读取cache并包装新的`LoadedSkill`；SessionExecutor/ActiveTurnTask在resolve await后验证candidate或active Turn target、control_generation、ConversationRevision和EmergencyControl basis；
- drop某个load waiter不发布部分LoadedSkill；shared parse/cache工作可以继续，但只有仍current且通过owner重验的resolve caller可以使用结果；
- 不同Session或source grant可以共享相同正文bytes，但不能共享错误provenance；
- cache不改变SkillMetadata或已经返回的SkillView；
- cache eviction不改变任何已返回view；
- shared或Workspace source重新publication后，future SkillView entry使用新的内部cache key；active Turn entry仍使用old captured bytes；
- 已经返回的 `Arc<SkillContent>` 和 `Arc<LoadedSkill>` 不被原地修改。

## Injection

Injection 层只消费已经加载的 Skill：

```rust
pub struct SkillContributionRef {
    skill_id: SkillId,
    source: SkillSourceRef,
}

pub struct SkillInjection {
    reference: SkillContributionRef,
    contribution: PromptContribution,
}

impl SkillContributionRef {
    pub fn skill_id(&self) -> &SkillId;
    pub fn source(&self) -> &SkillSourceRef;
}

pub struct SkillInjector;

impl SkillInjector {
    pub fn build(skill: &LoadedSkill) -> Result<SkillInjection, SkillInjectionError>;
}

impl SkillInjection {
    pub fn reference(&self) -> &SkillContributionRef;
    pub fn contribution(&self) -> &PromptContribution;
}
```

`LoadedSkill`、`SkillContent`、`SkillContributionRef`和`SkillInjection`字段/constructor保持private，只能由SkillService或SkillInjector创建。SkillInjector从`LoadedSkill`生成`SkillContributionRef`；该引用只保留SkillId和exact `SkillSourceRef`，用于composition前的captured-view与source authorization校验。校验成功后PromptSet只投影`Skill { skill_id }`safe part-level stamp；exact source ref不进入live/recorded provenance。正文正确性由已规范化的MessageRecord实际内容承担，不使用额外派生摘要证明。该边界消费[INV-202](../architecture.md#跨模块不变量索引)。

职责划分：

```text
SkillService
→ 找到并加载什么内容

Turn execution
→ 本轮需要哪个Skill，并使用捕获的SkillView entry

SkillInjector
→ 如何把 LoadedSkill 转换为 Prompt contribution

Prompt
→ 将contribution规范化进User/Steer MessageRecord
→ ActiveTurnTask apply到LiveConversation后，从sanitized live conversation组装模型输入
```

`SkillInjector`不执行discovery、view filtering、文件读取、cache lookup或Skill选择。

## 渐进披露流程

```text
MiniCoreRuntime 启动
→ 创建 Arc<SkillService>
→ SkillService.initialize()
→ 捕获authorized source bytes并解析轻量 metadata

Turn admission
→ 使用pinned WorkspaceSkillContext构造Arc<SkillViewContext>
→ SkillService.for_turn(captured SkillResourceView, context.clone())
→ 得到Arc<SkillView>
→ TurnExecutionContext捕获SkillService、view和同一个context Arc

输入规范化需要 Skill
→ PromptIntent.skills[]提供ordered SkillId
→ TurnExecutionContext从捕获的SkillView取得exact SkillEntry
→ await SkillService.load(&view, &entry)
→ cache hit，或从captured bytes解析并缓存完整内容
→ SkillInjector.build(loaded_skill)
→ 构造UserMessageCompositionInput
→ PromptSet同步验证一一匹配并compose_user_message(...)
→ live UserMessage / Steer + record attempt

模型触发的 Skill Tool
→ apply truthful tool message live + record attempt
→ 同一assistant全部matching ToolResult存在时形成complete exchange

下一次模型调用
→ 只从sanitized LiveConversationView组装
```

渐进披露阶段：

```text
阶段 1：SkillView metadata
阶段 2：selected Skill full content
阶段 3：Prompt contribution
```

## Reload

source watcher只标记dirty或reload available，不自动替换current resource root，也不更新captured bytes。shared Skill source由显式`/reload`发布：

```text
capture authorized source bytes
→ 从captured bytes解析metadata
→ 构建candidate SkillResourceView
→ 完整校验
→ 与Prompt/Tool/Model candidates一起原子替换current roots
→ future Turn捕获新view
```

Workspace Skill source不由shared `/reload`读取。Session load、Idle Workspace definition update或`/reload workspace`在publication前捕获authorized Workspace Skill bytes；成功后future Turn的`WorkspaceSkillContext`携带new captured sources。initial load失败进入Unavailable；definition update失败保留old definition/Snapshot；Ready reload失败保留old Snapshot；Unavailable reload失败保持Unavailable。

基础不变量：

- shared reload失败时继续保留旧SkillResourceView；Workspace candidate失败按initial load、definition update、Ready reload或Unavailable retry的对应Session lifecycle规则处理；
- reload不修改已经返回的`Arc<LoadedSkill>`；
- 已经生成并固化到live/recorded MessageRecord的safe part-level contribution stamp不被回写；
- active Turn继续使用捕获的旧SkillView；
- 尚未加载的entry只解析捕获view中的immutable bytes，不能看到reload之后但未发布的文件当前内容；
- shared source删除后，必须等显式`/reload`成功，future view才不再披露该Skill；Workspace source删除后必须等`/reload workspace`、Idle definition update或下一次Session load成功。旧view、captured bytes和已有不可变引用按持有者生命周期释放。

## 错误和 Diagnostics

基础错误分类：

```rust
pub enum SkillErrorKind {
    Discovery,
    MetadataParse,
    DuplicateName,
    NotFound,
    ContentRead,
    ContentParse,
    Injection,
    Invalidated,
}
```

SkillService保存结构化diagnostics。SkillView可以包含有效entries并同时报告非致命diagnostics；请求加载某个失败Skill时返回对应typed error。

## 领域所有权

| 对象 | Owner |
| --- | --- |
| `Arc<SkillService>` 生命周期 | MiniCoreRuntime |
| `Arc<SkillService>` execution 引用 | loaded Session execution / TurnExecutionContext |
| Skill discovery/filter/cache/invalidation | SkillService |
| SkillResourceView candidate build/validation、SkillView创建和cache | SkillService |
| complete SharedResourceRoots publication | MiniCoreRuntime |
| 本Turn捕获的SkillService/SkillViewContext/SkillView binding | TurnExecutionContext |
| Skill metadata | SkillService / SkillView |
| 完整Skill内容 | SkillService content cache |
| 某次执行选择哪个Skill | Turn execution |
| entry来源和读取授权校验 | TurnExecutionContext + SkillService |
| Skill 到 Prompt contribution 的转换 | SkillInjector |
| contribution到live MessageRecord | PromptSet输入规范化 |
| 最终模型上下文组装 | PromptSet |

## 基础不变量

- 一个 Runtime 初始化一个 SkillService；
- durable Session 不持有 SkillService；Runtime 将同一个 SkillService 注入 loaded execution；
- Turn 对象不持有 Skill；
- SkillView的模型可见投影不包含完整正文；entry私有持有captured source bytes；
- 完整正文只能通过SkillService从captured bytes按需解析；
- Turn execution不能直接读取Skill文件；
- TurnExecutionContext捕获`Arc<SkillService>`、`Arc<SkillViewContext>`和绑定该context的`Arc<SkillView>`；
- Workspace Skill的capture只能通过携带该context的source adapter seam，load只能解析captured entry bytes；
- SkillInjector不能决定选择哪个Skill；
- SkillContributionRef把SkillId和exact source authorization贯穿到composition前校验；PromptSet只把SkillId投影为safe part-level stamp；
- Prompt 不能执行 Skill discovery 或 load；
- Input与Steer只通过TurnExecutionContext async `resolve_user_message()`加载Skill；PromptSet composition保持同步纯内存；
- 用户侧显式SkillIntent/SkillInjection必须进入live UserMessage或Steer并完成inline record attempt，不创建独立Item；模型触发的Skill Tool创建ToolInvocation Item，输出进入live role=tool message，并在同一assistant全部ToolCall拥有matching result后随complete exchange进入conversation，不能作为current-call旁路；
- SkillService 不决定哪个 Turn 使用哪个 Skill；
- cache和load state不进入领域对象；
- source变化只标记dirty；shared current root只在显式`/reload`成功后替换，Workspace captured sources只在Session-local candidate publication后替换；
- active使用中的view和不可变内容不被reload原地修改。

## 已关闭问题

5. **Skill invocation与Item边界已由ADR 0129关闭**：用户显式Skill选择属于UserMessage/Steer规范化，不创建Item；模型触发的Skill Tool继续创建ToolInvocation Item。
6. **SkillInjection、UserMessageCompositionInput和recorded stamp格式已由ADR 0129关闭**：SkillIntent只保存SkillId；exact source ref只做process-local校验；每个contribution对应一个顶层part和safe stamp。
7. **async composition seam已由ADR 0130关闭**：TurnExecutionContext绑定SkillService/context/view，Session execution在Starting/Steer await前后处理Cancel、SecurityRevoked和basis重验，PromptSet不执行load。

## 后续问题

1. SkillMetadata的完整字段。
2. Agent、Session 与 Workspace Skill source 的 scope precedence 和 filtering 规则。
3. SkillScope 的精确定义。
4. SkillName 冲突、namespace 和稳定排序规则。
7. source watcher的dirty notification和debounce行为。
8. cache容量、eviction和失败重试策略。
9. SkillService 初始化失败对 Runtime 启动的影响。
10. Session reload 后如何重新获得同一个 SkillService。

## 设计进度

- [x] 确定 MiniCoreRuntime 初始化并拥有一个 `Arc<SkillService>`。
- [x] 确定 durable Session 不持有 SkillService，Runtime 注入 loaded execution。
- [x] 确定Turn对象不持有Skill、SkillView、LoadedSkill或Skill snapshot。
- [x] 确定SkillView只包含轻量metadata，不包含完整正文。
- [x] 确定完整 Skill 内容由 SkillService 按需加载、解析和缓存。
- [x] 确定 SkillService 管理 discovery、filtering、cache、invalidation 和 diagnostics。
- [x] 确定 Turn execution 决定本次执行使用哪个 Skill。
- [x] 确定TurnExecutionContext捕获SkillService、Arc<SkillViewContext>和绑定该context的Arc<SkillView>。
- [x] 确定 SkillInjector 只负责 LoadedSkill 到 PromptContribution 的转换。
- [x] 确定用户侧Skill contribution必须固化到User/Steer entry；Skill Tool输出通过tool message和complete Tool exchange进入conversation。
- [x] 确定reload和cache eviction不修改已经返回的不可变LoadedSkill。
- [ ] 定义 SkillMetadata 和 SkillContent 的最终字段。
- [x] 定义SkillViewContext的Session/Agent identity与WorkspaceSkillContext输入。
- [x] 确定load使用captured SkillEntry，不查询reload后的shared root或future SkillView。
- [x] 确定Workspace Skill source adapter通过capture context强制校验location和exact source authorization；Session lifecycle负责operation/control validation，lazy load只解析captured bytes。
- [x] 确定Runtime中的SkillResourceView root只在显式`/reload`成功后替换，per-Turn SkillView从captured roots构建。
- [ ] 定义 SkillScope。
- [ ] 定义 SkillName 冲突和 namespace 规则。
- [x] 定义用户显式Skill invocation不创建Item、模型Skill Tool创建ToolInvocation Item，以及UserMessageCompositionInput和safe part-level PromptContribution stamp最终形状。
- [x] 定义Input/Steer共享async resolve seam、load waiter cancellation与await后owner重验（ADR 0130）。
- [ ] 定义 watcher、reload、cache eviction 和失败重试策略。
