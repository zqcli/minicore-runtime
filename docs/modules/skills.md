# Skill 子系统架构设计

状态：当前权威架构（设计已冻结，实现进行中）
日期：2026-07-16

## 目的

本文定义MiniCore Skill子系统的基础对象、所有权、渐进披露、按需加载、缓存、reload view和Prompt Injection关系。

本文以以下关系为基础：

```text
MiniCoreRuntime 初始化一个 Arc<SkillService>
Runtime 将该 Service 注入 loaded Session execution / TurnExecutionContext
Turn 领域对象不持有 Skill、SkillView、LoadedSkill 或 TurnSkills
SkillService按context发布不可变current SkillView
TurnExecutionContext捕获view，并按entry location延迟加载Skill
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
- SkillService负责Skill发现、metadata解析、过滤、SkillView构建、完整内容加载、解析、缓存、reload和diagnostics；
- SkillView是轻量、可重建、发布后不可变的metadata view；
- SkillView不包含完整Skill内容；
- Skill 内容默认按需加载；
- Turn 对象不保存任何 Skill 字段；
- 哪一个 Turn 在执行期间需要哪个 Skill，由 Turn execution 决定；
- TurnExecutionContext使用捕获的SkillView及其context通过SkillService请求Skill；
- `SkillInjector` 只负责把已经加载的 Skill 转换成 Prompt contribution；
- Prompt 负责把 Skill contribution 与其他输入组装成最终模型上下文；
- 已加载 Skill 内容是不可变值；
- cache eviction或reload不会修改已经返回的`Arc<LoadedSkill>`；
- reload成功后原子替换current SkillView，只影响future Turn；已加载内容和已经开始的模型调用不回写。

## 对象关系

```text
MiniCoreRuntime
└─ Arc<SkillService>
   ├─ SkillSourceAdapter*
   ├─ current SkillView cache
   ├─ SkillContentCache
   ├─ SkillLoadStateStore
   └─ SkillDiagnostics

loaded Session execution
└─ Runtime 注入 Arc<SkillService>

TurnExecutionContext
├─ SkillViewContext
├─ Arc<SkillView>
└─ SkillService::load(&context, &entry)
      └─ Arc<LoadedSkill>
         └─ SkillInjector
            └─ PromptContribution
               └─ committed input normalization
```

## MiniCoreRuntime

`MiniCoreRuntime` 是 SkillService 的创建者和生命周期 owner：

```rust
pub struct MiniCoreRuntime {
    pub prompt_service: Arc<PromptService>,
    pub tools: Arc<ToolService>,
    pub skills: Arc<SkillService>,
}
```

Runtime 启动时：

1. 创建 Skill source adapters；
2. 创建 `SkillService`；
3. 初始化基础discovery和current SkillView；
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

Turn执行期间若确定需要某个Skill，TurnExecutionContext使用本Turn捕获的SkillView entry和`WorkspaceSkillContext`请求完整内容。加载结果进入Injection和输入规范化流程，不写回Turn对象。load不重新查询reload后的current view，也不能把SkillInjection作为未提交的current-call Prompt旁路。完整capture、input composition和reload规则见[Turn执行模块与执行上下文架构设计](turn-execution-context.md)。

## SkillService

`SkillService`是Skill子系统的深模块，对外隐藏discovery、文件读取、解析、过滤、cache、并发去重、reload和diagnostics：

```rust
pub struct SkillService {
    sources: Vec<Arc<dyn SkillSourceAdapter>>,
    views: SkillViewCache,
    content_cache: SkillContentCache,
    load_states: SkillLoadStateStore,
    diagnostics: SkillDiagnostics,
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
    pub async fn initialize(&self) -> Result<(), SkillError>;

    pub async fn current_view(
        &self,
        context: SkillViewContext,
    ) -> Result<Arc<SkillView>, SkillError>;

    pub async fn reload(
        &self,
        context: SkillViewContext,
    ) -> Result<Arc<SkillView>, SkillError>;

    pub async fn load(
        &self,
        context: &SkillViewContext,
        entry: &SkillEntry,
    ) -> Result<Arc<LoadedSkill>, SkillLoadError>;

}
```

`current_view()`只返回已发布metadata view；`reload()`完整构建candidate view，成功后原子替换同一context的current view，失败时保留旧view。`load()`才允许读取和解析完整正文。

`load()`必须：

- 使用Turn捕获的`SkillEntry.location`定位source；
- 使用与该view相同的`SkillViewContext`；
- 在实际读取时校验Workspace authorization lease和source stamp仍有效；
- 不重新查询reload后的current view。

本设计采用常规弱一致性：尚未加载的entry在文件变化后可能读到新正文；已经返回的`Arc<LoadedSkill>`和已经committed的UserMessage不被修改。

## Skill Source Adapter

不同物理来源通过内部 adapter 接入 SkillService：

```rust
pub trait SkillSourceAdapter: Send + Sync {
    async fn discover(
        &self,
        context: &SkillViewContext,
    ) -> Result<Vec<DiscoveredSkill>, SkillSourceError>;

    async fn read(
        &self,
        request: &SkillSourceReadRequest<'_>,
    ) -> Result<Vec<u8>, SkillSourceError>;
}

pub struct SkillSourceReadRequest<'a> {
    pub context: &'a SkillViewContext,
    pub entry: &'a SkillEntry,
}
```

Workspace Skill source adapter只能使用`SkillViewContext.workspace`中的authorized source roots，并在discover/read时校验authorization lease和source stamp。BuiltIn/User adapter可以忽略Workspace roots，但不能伪造Workspace source stamp。本设计不定义BuiltIn/User/Workspace/Agent/Session等来源层级和优先级。

```rust
pub struct DiscoveredSkill {
    pub location: SkillLocation,
    pub modified_at: Option<Timestamp>,
    pub source_stamp: SkillSourceAuthorizationStamp,
}
```

## Skill View

SkillView是渐进披露的第一阶段，只包含轻量metadata：

```rust
pub struct SkillView {
    pub fingerprint: SkillViewFingerprint,
    entries: Arc<[SkillEntry]>,
}

pub struct SkillEntry {
    pub metadata: SkillMetadata,
    pub location: SkillLocation,
    pub source_stamp: SkillSourceAuthorizationStamp,
}

pub struct SkillMetadata {
    pub id: SkillId,
    pub name: SkillName,
    pub description: String,
    pub path: PathBuf,
    pub scope: SkillScope,
}
```

`SkillView::entries()`返回不可变entry。Turn捕获`Arc<SkillView>`后不再查询current view；模型可见projection只暴露必要metadata，省略location和source stamp。

`SkillScope` 只作为 metadata 和 filtering 输入，不定义层级、覆盖或优先级语义：

```rust
pub struct SkillScope;
```

SkillView必须满足：

- 不包含Skill正文；
- fingerprint覆盖稳定排序后的entry和模型可见metadata，仅用于当前view一致性、cache和diagnostics；
- metadata顺序确定；
- `SkillId`稳定；
- `SkillName`冲突产生明确diagnostics；
- reload可以从Skill sources重建candidate view，并在成功后原子替换current view；
- UI可见view与模型可见view可以使用不同安全投影。

模型披露建议只暴露必要字段，避免自动泄漏本地绝对路径：

```rust
pub struct ModelVisibleSkillMetadata {
    pub id: SkillId,
    pub name: SkillName,
    pub description: String,
}

impl SkillView {
    pub fn prompt_view(&self) -> SkillPromptView;
}
```

`SkillPromptView`只包含稳定排序后的模型安全metadata和`SkillViewFingerprint`。

## 完整 Skill 内容

完整内容只在 `load()` 后产生：

```rust
pub struct LoadedSkill {
    pub metadata: SkillMetadata,
    pub source: SkillSourceRef,
    pub content: Arc<SkillContent>,
}

pub struct SkillContent {
    pub body: String,
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
- content cache保存不可变`Arc<SkillContent>`，不缓存带某个Session source stamp的完整LoadedSkill；
- load失败进入diagnostics，并允许后续重试；
- 默认baseline是metadata discovery + content lazy load；
- 不引入Eager/Lazy/Manual多级LoadPolicy。

## Cache

内容cache使用内部文件状态作为key：

```text
SkillCacheKey {
    location,
    source_stamp,
    file_state,
}
```

cache 规则：

- 同一个 key 返回同一个或等价的不可变 `Arc<SkillContent>`；
- content cache不保存authorization lease；
- 每次`load()`都校验entry来源、source stamp和authorization lease，再包装新的`LoadedSkill`；
- 不同Session或source grant可以共享相同正文bytes，但不能共享错误provenance；
- cache不改变SkillMetadata或已发布SkillView；
- cache eviction不改变current view；
- reload或文件状态变化后，后续load使用新的内部cache key；
- 已经返回的 `Arc<SkillContent>` 和 `Arc<LoadedSkill>` 不被原地修改。

## Injection

Injection 层只消费已经加载的 Skill：

```rust
pub struct SkillContributionRef {
    pub skill_id: SkillId,
    pub source_stamp: SkillSourceAuthorizationStamp,
}

pub struct SkillInjection {
    pub reference: SkillContributionRef,
    pub contribution: PromptContribution,
}

pub struct SkillInjector;

impl SkillInjector {
    pub fn build(skill: &LoadedSkill) -> Result<SkillInjection, SkillInjectionError>;
}
```

SkillInjector从`LoadedSkill`生成`SkillContributionRef`。该引用只保留SkillId和source stamp，用于committed UserMessage provenance；正文完整性由最终CanonicalUserMessage自身的fingerprint覆盖。

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
→ Session execution append/apply后，从committed conversation组装模型输入
```

`SkillInjector`不执行discovery、view filtering、文件读取、cache lookup或Skill选择。

## 渐进披露流程

```text
MiniCoreRuntime 启动
→ 创建 Arc<SkillService>
→ SkillService.initialize()
→ 发现并解析轻量 metadata

Turn admission
→ 使用pinned WorkspaceSkillContext构造SkillViewContext
→ SkillService.current_view(...)
→ 得到Arc<SkillView>
→ TurnExecutionContext捕获view和context

输入规范化需要 Skill
→ TurnExecutionContext.compose_message(PromptIntent)
→ 从捕获的SkillView取得SkillEntry
→ SkillService.load(&context, &entry)
→ cache hit，或读取、解析并缓存完整内容
→ SkillInjector.build(loaded_skill)
→ PromptSet.compose_user_message(...)
→ committed UserMessage / Steer

模型触发的 Skill Tool
→ append truthful tool message
→ append tool_round_completed

下一次模型调用
→ 只从 committed conversation 组装
```

渐进披露阶段：

```text
阶段 1：SkillView metadata
阶段 2：selected Skill full content
阶段 3：Prompt contribution
```

## Reload

source watcher只标记dirty或reload available，不自动替换current view。显式reload流程：

```text
discover/read metadata
→ 构建candidate SkillView
→ 完整校验
→ 成功后原子替换同一context的current view
→ future Turn捕获新view
```

基础不变量：

- reload失败时继续保留旧current view；
- reload不修改已经返回的`Arc<LoadedSkill>`；
- 已经生成并固化到committed MessageRecord的contribution stamp不被回写；
- active Turn继续使用捕获的旧SkillView；
- 尚未加载的entry按捕获view中的location读取，允许看到文件当前内容；
- source删除后，新view不再披露该Skill；旧view和已有不可变引用按持有者生命周期释放。

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
| SkillView创建、publication和cache | SkillService |
| 本Turn捕获的SkillView | TurnExecutionContext |
| Skill metadata | SkillService / SkillView |
| 完整Skill内容 | SkillService content cache |
| 某次执行选择哪个Skill | Turn execution |
| entry来源和读取授权校验 | TurnExecutionContext + SkillService |
| Skill 到 Prompt contribution 的转换 | SkillInjector |
| contribution 到 committed MessageRecord | PromptSet 输入规范化 |
| 最终模型上下文组装 | PromptSet |

## 基础不变量

- 一个 Runtime 初始化一个 SkillService；
- durable Session 不持有 SkillService；Runtime 将同一个 SkillService 注入 loaded execution；
- Turn 对象不持有 Skill；
- SkillView不包含完整正文；
- 完整正文只能通过SkillService按需加载；
- Turn execution不能直接读取Skill文件；
- TurnExecutionContext捕获SkillViewContext和`Arc<SkillView>`；
- Workspace Skill的discover/read只能通过携带该context和SkillEntry的source adapter seam；
- SkillInjector不能决定选择哪个Skill；
- SkillContributionRef把SkillId和source stamp贯穿到Prompt contribution；
- Prompt 不能执行 Skill discovery 或 load；
- 用户侧SkillInjection必须进入append/applied UserMessage或Steer；模型触发的Skill Tool输出进入role=tool message并由`tool_round_completed`promote，不能作为current-call旁路；
- SkillService 不决定哪个 Turn 使用哪个 Skill；
- cache和load state不进入领域对象；
- source变化只标记dirty；current view只在显式reload成功后原子替换；
- active使用中的view和不可变内容不被reload原地修改。

## 后续问题

1. SkillMetadata的完整字段。
2. Agent、Session 与 Workspace Skill source 的 scope precedence 和 filtering 规则。
3. SkillScope 的精确定义。
4. SkillName 冲突、namespace 和稳定排序规则。
5. Skill invocation 如何触发，以及是否形成 Item。
6. SkillInjection、UserMessageCompositionInput 和 committed contribution stamp 的最终格式。
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
- [x] 确定TurnExecutionContext捕获SkillViewContext、Arc<SkillView>和fingerprint。
- [x] 确定 SkillInjector 只负责 LoadedSkill 到 PromptContribution 的转换。
- [x] 确定用户侧Skill contribution必须固化到User/Steer entry；Skill Tool输出通过tool message + `tool_round_completed`进入conversation。
- [x] 确定reload和cache eviction不修改已经返回的不可变LoadedSkill。
- [ ] 定义 SkillMetadata 和 SkillContent 的最终字段。
- [x] 定义SkillViewContext的Session/Agent identity与WorkspaceSkillContext输入。
- [x] 确定load使用captured SkillEntry，不重新查询reload后的current view。
- [x] 确定Workspace Skill source adapter在discover/read时强制校验context、lease和source stamp。
- [x] 确定current SkillView只在显式reload成功后原子替换。
- [ ] 定义 SkillScope。
- [ ] 定义 SkillName 冲突和 namespace 规则。
- [ ] 定义 invocation、Item、UserMessageCompositionInput 和 PromptContribution stamp 的最终形状。
- [ ] 定义 watcher、reload、cache eviction 和失败重试策略。
