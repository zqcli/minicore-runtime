# Skill 子系统架构设计

状态：基础架构已确定；实现细节待补充
日期：2026-07-16

## 目的

本文定义 MiniCore Skill 子系统的基础对象、所有权、渐进披露、按需加载、缓存、失效和 Prompt Injection 关系。

本文以以下关系为基础：

```text
MiniCoreRuntime 初始化一个 Arc<SkillService>
Runtime 将该 Service 注入 loaded Session execution / TurnExecutionContext
Turn 领域对象不持有 Skill、Skill Catalog、LoadedSkill 或 TurnSkills
TurnExecutionContext pin SkillCatalog，并通过 exact reference 延迟加载 Skill
Injection 层把已加载 Skill 转换为本轮 Prompt contribution
```

本阶段暂不设计：

- BuiltIn、Runtime、User、Workspace、Agent、Session 等 Skill 层级；
- 不同 Skill 来源之间的优先级、覆盖和 namespace 规则；
- Skill 配置的持久化格式；
- Skill invocation 的 protocol method 和 Item 表达；
- 文件监听器、远程 Skill source 或插件协议的具体实现；
- manager、actor 或 storage 的最终划分。

## 决策摘要

已经确定：

- `SkillService` 在 `MiniCoreRuntime` 启动时初始化；
- 一个 Runtime 内的 loaded Session execution 共享同一个 `Arc<SkillService>`；
- durable Session/SessionDefinition 不持有 Service handle，也不复制 Skill definitions、references、Catalog entries 或正文；
- SkillService 负责 Skill 发现、metadata 解析、过滤、Catalog 构建、完整内容加载、解析、缓存、失效和 diagnostics；
- Skill Catalog 是轻量、可重建的 metadata snapshot；
- Catalog 不包含完整 Skill 内容；
- Skill 内容默认按需加载；
- Turn 对象不保存任何 Skill 字段；
- 哪一个 Turn 在执行期间需要哪个 Skill，由 Turn execution 决定；
- TurnExecutionContext 使用 pinned Catalog context 通过 SkillService 请求 Skill；
- `SkillInjector` 只负责把已经加载的 Skill 转换成 Prompt contribution；
- Prompt 负责把 Skill contribution 与其他输入组装成最终模型上下文；
- 已加载 Skill 内容是不可变值；
- cache 失效不会修改已经返回的 `Arc<LoadedSkill>`；
- Catalog 或内容失效影响后续 catalog/load 结果，不回写已经开始的模型调用。

## 对象关系

```text
MiniCoreRuntime
└─ Arc<SkillService>
   ├─ SkillSourceAdapter*
   ├─ SkillCatalogCache
   ├─ SkillContentCache
   ├─ SkillLoadStateStore
   └─ SkillDiagnostics

loaded Session execution
└─ Runtime 注入 Arc<SkillService>

TurnExecutionContext
├─ pinned SkillCatalogContext
├─ Arc<SkillCatalog>
└─ SkillService::load(SkillLoadRequest { pinned context, pinned entry })
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
3. 初始化基础 discovery 和 metadata Catalog；
4. 将同一个 `Arc<SkillService>` 注入后续 loaded Session execution / Turn capture；
5. Runtime shutdown 时停止新的 discovery/load，并释放子系统资源。

## Session

Durable Session 和 SessionDefinition 不保存 SkillService、Catalog 或正文：

```text
Session / SessionDefinition
≠ Arc<SkillService>
≠ SkillCatalog
≠ LoadedSkill
≠ Skill cache
```

Runtime 在 load/capture 时把同一个 SkillService 注入 Session execution。Turn admission 使用 exact AgentRevisionRef、SessionDefinitionRevision 和本 Turn pin 的 WorkspaceSkillContext 构造 SkillCatalogContext；SkillService 不能从 SessionId 回查 current SessionDefinition 或自行扩大 Workspace Skill source。

完整 Agent/Session lifecycle 见 [Agent 与 Session 生命周期架构设计](agent-session-lifecycle.md)，Workspace 授权见 [Workspace 子系统架构设计](workspace-subsystem.md)。

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
SkillCatalog
CatalogRevision
SkillRef
SkillId[]
LoadedSkill[]
```

Turn 执行期间若确定需要某个 Skill，TurnExecutionContext 使用本 Turn 已捕获的 SkillCatalog entry 和 `WorkspaceSkillContext` 请求完整内容。加载结果进入 Injection 和输入规范化流程，不写回 Turn 对象。Skill load 不能通过 `skill_id + current Session context` 重新解析 future Catalog，也不能把 SkillInjection 作为未提交的 current-call Prompt 旁路。完整 pinning、input composition 和 reload 规则见 [Turn 执行模块与执行上下文架构设计](turn-execution-context.md)。

## SkillService

`SkillService` 是 Skill 子系统的深模块，对外隐藏 discovery、文件读取、解析、过滤、cache、并发去重、失效和 diagnostics：

```rust
pub struct SkillService {
    sources: Vec<Arc<dyn SkillSourceAdapter>>,
    catalog_cache: SkillCatalogCache,
    content_cache: SkillContentCache,
    load_states: SkillLoadStateStore,
    diagnostics: SkillDiagnostics,
}
```

Catalog context 固定当前 Agent、Session 和 Turn-pinned Workspace Skill view：

```rust
pub struct SkillCatalogContext {
    pub agent: AgentRevisionRef,
    pub session_id: SessionId,
    pub session_revision: SessionDefinitionRevision,
    pub workspace: WorkspaceSkillContext,
}

pub struct SkillLoadRequest {
    pub context: SkillCatalogContext,
    pub entry: SkillCatalogEntryRef,
}
```

基础 interface：

```rust
impl SkillService {
    pub async fn initialize(&self) -> Result<(), SkillError>;

    pub async fn catalog(
        &self,
        context: SkillCatalogContext,
    ) -> Result<Arc<SkillCatalog>, SkillError>;

    pub async fn load(
        &self,
        request: SkillLoadRequest,
    ) -> Result<Arc<LoadedSkill>, SkillLoadError>;

    pub async fn invalidate(
        &self,
        request: SkillInvalidation,
    ) -> Result<(), SkillError>;
}
```

`catalog()` 只返回 metadata。`load()` 才允许读取和解析完整正文。

`load()` 必须：

- 使用 Catalog 返回的 `SkillCatalogEntryRef` 精确定位 source；
- 使用与该 Catalog 相同的 pinned `SkillCatalogContext`；
- 校验 Workspace authorization lease 仍有效；
- 校验 entry 的 version、content hash、location 和 source stamp；
- 不重新查询 current Catalog；
- 不把相同 `SkillId` 静默解析到更新后的正文。

## Skill Source Adapter

不同物理来源通过内部 adapter 接入 SkillService：

```rust
pub trait SkillSourceAdapter: Send + Sync {
    async fn discover(
        &self,
        context: &SkillCatalogContext,
    ) -> Result<Vec<DiscoveredSkill>, SkillSourceError>;

    async fn read(
        &self,
        request: &SkillSourceReadRequest<'_>,
    ) -> Result<Vec<u8>, SkillSourceError>;
}

pub struct SkillSourceReadRequest<'a> {
    pub context: &'a SkillCatalogContext,
    pub entry: &'a SkillCatalogEntryRef,
}
```

Workspace Skill source adapter 只能使用 `SkillCatalogContext.workspace` 中的 authorized source roots，并在 discover/read 时校验 authorization lease 和 source stamp。BuiltIn/User adapter 可以忽略 Workspace roots，但不能伪造 Workspace source stamp。本阶段仍不定义 BuiltIn/User/Workspace/Agent/Session 等来源层级和优先级。

```rust
pub struct DiscoveredSkill {
    pub location: SkillLocation,
    pub modified_at: Option<Timestamp>,
    pub content_hash: Option<ContentHash>,
    pub source_stamp: SkillSourceAuthorizationStamp,
}
```

## Skill Catalog

Catalog 是渐进披露的第一阶段，只包含轻量 metadata：

```rust
pub struct SkillCatalog {
    pub revision: CatalogRevision,
    pub fingerprint: SkillCatalogFingerprint,
    entries: Arc<[SkillCatalogEntry]>,
}

pub struct SkillCatalogEntry {
    pub metadata: SkillMetadata,
    pub reference: SkillCatalogEntryRef,
}

pub struct SkillMetadata {
    pub id: SkillId,
    pub version: DefinitionVersion,
    pub name: SkillName,
    pub description: String,
    pub path: PathBuf,
    pub scope: SkillScope,
    pub content_hash: ContentHash,
}

pub struct SkillCatalogEntryRef {
    pub catalog_revision: CatalogRevision,
    pub id: SkillId,
    pub version: DefinitionVersion,
    pub content_hash: ContentHash,
    pub location: SkillLocation,
    pub source_stamp: SkillSourceAuthorizationStamp,
}
```

`SkillCatalogEntryRef` 是 lazy load 的 pinned identity。`SkillCatalog::entries()` 返回同时包含 metadata 和 reference 的不可变 entry；调用方不需要按 SkillId 回查 current Catalog。模型可见 projection 只暴露必要 metadata，省略 location 和 source stamp。

`SkillScope` 在当前阶段只作为 metadata 和 filtering 输入，不定义层级、覆盖或优先级语义：

```rust
pub struct SkillScope;
```

Catalog 必须满足：

- 不包含 Skill 正文；
- fingerprint 覆盖稳定排序后的完整 SkillCatalogEntryRef 和模型可见 metadata；
- metadata 顺序确定；
- `SkillId` 稳定；
- `SkillName` 冲突必须产生明确 diagnostics；
- Catalog revision 在有效 metadata 集合变化时改变；
- Catalog 可以从 Skill sources 重新构建；
- UI 可见 Catalog 与模型可见 Catalog 可以使用不同安全投影。

模型披露建议只暴露必要字段，避免自动泄漏本地绝对路径：

```rust
pub struct ModelVisibleSkillMetadata {
    pub id: SkillId,
    pub name: SkillName,
    pub description: String,
}
```

## 完整 Skill 内容

完整内容只在 `load()` 后产生：

```rust
pub struct LoadedSkill {
    pub metadata: SkillMetadata,
    pub reference: SkillCatalogEntryRef,
    pub content: Arc<SkillContent>,
}

pub struct SkillContent {
    pub body: String,
}
```

精确定义身份为：

```text
SkillId + DefinitionVersion + ContentHash
```

正文变化必须形成新的 version 或 content hash。加载状态变化不改变 Skill 定义身份。

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

- Catalog metadata 可用不代表正文已经加载；
- 同一正文 identity 的并发 load 应合并为一次底层读取和解析；
- content cache 保存不可变 `Arc<SkillContent>`，不缓存带某个 Session source stamp 的完整 LoadedSkill；
- load 失败进入 diagnostics，并允许后续按失效或重试规则重新加载；
- 默认 baseline 是 metadata discovery + content lazy load；
- 当前阶段不引入 Eager/Lazy/Manual 多级 LoadPolicy；Skill 不使用领域模型中仅适用于 PromptDefinition 的 `DefinitionOverrides.load_policy`。

## Cache

内容 cache 使用精确定义身份作为 key：

```text
SkillCacheKey {
    skill_id,
    version,
    content_hash,
}
```

cache 规则：

- 同一个 key 返回同一个或等价的不可变 `Arc<SkillContent>`；
- content cache 不保存 `SkillCatalogEntryRef`、source stamp 或 authorization lease；
- 每次 `load()` 都先校验 request 中的 pinned entry、source stamp 和 authorization lease，再用 request 的 metadata/reference 包装新的 `LoadedSkill`；
- 不同 Session 或 source grant 可以共享相同正文 bytes，但不能共享错误 provenance；
- cache 不改变 SkillMetadata；
- cache eviction 不改变 Catalog；
- Catalog metadata 指向新 content hash 时，后续 load 使用新 key；
- 已经返回的 `Arc<SkillContent>` 和 `Arc<LoadedSkill>` 不被原地修改。

## Injection

Injection 层只消费已经加载的 Skill：

```rust
pub struct SkillContributionRef {
    pub catalog_revision: CatalogRevision,
    pub skill_id: SkillId,
    pub version: DefinitionVersion,
    pub content_hash: ContentHash,
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

SkillInjector 从 `LoadedSkill.reference` 生成 `SkillContributionRef`。该引用必须进入 PromptContribution source/stamp 和最终 fingerprint，使 TurnExecutionContext 与 PromptSet 可以验证 Catalog entry、LoadedSkill 正文与注入内容来自同一 pinned identity。

职责划分：

```text
SkillService
→ 找到并加载什么内容

Turn execution
→ 本轮需要哪个 Skill，并保证使用 pinned Catalog entry

SkillInjector
→ 如何把 LoadedSkill 转换为 Prompt contribution

Prompt
→ 将contribution规范化进User/Steer MessageRecord
→ Session execution append/apply后，从committed conversation组装模型输入
```

`SkillInjector` 不执行 discovery、Catalog filtering、文件读取、cache lookup 或 Skill 选择。

## 渐进披露流程

```text
MiniCoreRuntime 启动
→ 创建 Arc<SkillService>
→ SkillService.initialize()
→ 发现并解析轻量 metadata

Turn admission
→ 使用 pinned WorkspaceSkillContext 构造 SkillCatalogContext
→ SkillService.catalog(...)
→ 得到 Arc<SkillCatalog>
→ TurnExecutionContext pin Catalog 和 context

输入规范化需要 Skill
→ TurnExecutionContext.compose_message(PromptIntent)
→ 从 pinned SkillCatalog 取得 SkillCatalogEntryRef
→ SkillService.load(SkillLoadRequest { pinned context, pinned entry })
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
阶段 1：Catalog metadata
阶段 2：selected Skill full content
阶段 3：Prompt contribution
```

## 失效

失效可以由显式 reload、source 变化、metadata 变化或 content hash 变化触发：

```text
invalidate
→ 标记受影响 Catalog/cache entry 失效
→ 重建 Catalog 或在下一次访问时重建
→ 新 Catalog revision
→ 后续 load 使用新的精确定义 key
```

基础不变量：

- 失效不修改已经返回的 `Arc<LoadedSkill>`；
- 已经生成并固化到 committed MessageRecord 的 contribution stamp 不被回写；
- future Turn 的新 catalog/load 请求必须看到失效后的结果；
- active Turn 的 pinned entry 只能加载其 exact content hash；ordinary invalidation 后旧正文仍可从 content-addressed cache/source 获得时继续加载，无法获得时 fail closed，不能漂移到新 Catalog；
- 失效失败进入 diagnostics，不静默返回已知错误内容作为新版本；
- source 删除后 Catalog 不再披露该 Skill；已有不可变引用的生命周期由持有者自然结束。

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

SkillService 保存结构化 diagnostics。Catalog 可以返回有效 entries 和非致命 diagnostics；请求加载某个失败 Skill 时返回对应 typed error。

## 领域所有权

| 对象 | Owner |
| --- | --- |
| `Arc<SkillService>` 生命周期 | MiniCoreRuntime |
| `Arc<SkillService>` execution 引用 | loaded Session execution / TurnExecutionContext |
| Skill discovery/filter/cache/invalidation | SkillService |
| Skill Catalog 创建和 cache | SkillService |
| 本 Turn 的 pinned SkillCatalog | TurnExecutionContext |
| Skill metadata | SkillService / Catalog |
| 完整 Skill 内容 | SkillService content cache |
| 某次执行选择哪个 Skill | Turn execution |
| pinned entry 校验和 exact load | TurnExecutionContext + SkillService |
| Skill 到 Prompt contribution 的转换 | SkillInjector |
| contribution 到 committed MessageRecord | PromptSet 输入规范化 |
| 最终模型上下文组装 | PromptSet |

## 基础不变量

- 一个 Runtime 初始化一个 SkillService；
- durable Session 不持有 SkillService；Runtime 将同一个 SkillService 注入 loaded execution；
- Turn 对象不持有 Skill；
- Catalog 不包含完整正文；
- 完整正文只能通过 SkillService 按需加载；
- Turn execution 不能直接读取 Skill 文件；
- TurnExecutionContext pin SkillCatalogContext、SkillCatalog 和 SkillCatalogFingerprint；
- Workspace Skill 的 discover/read 只能通过携带 pinned SkillCatalogContext 和 SkillCatalogEntryRef 的 source adapter seam；
- SkillInjector 不能决定选择哪个 Skill；
- SkillContributionRef 必须把 catalog revision、version、content hash 和 source stamp 贯穿到 Prompt contribution；
- Prompt 不能执行 Skill discovery 或 load；
- 用户侧SkillInjection必须进入append/applied UserMessage或Steer；模型触发的Skill Tool输出进入role=tool message并由`tool_round_completed`promote，不能作为current-call旁路；
- SkillService 不决定哪个 Turn 使用哪个 Skill；
- cache 和 load state 不进入领域对象；
- active 使用中的不可变内容不被 reload 原地修改。

## 后续问题

1. SkillMetadata 的完整字段和 `DefinitionVersion` 生成方式。
2. Agent、Session 与 Workspace Skill source 的 scope precedence 和 filtering 规则。
3. SkillScope 的精确定义。
4. SkillName 冲突、namespace 和稳定排序规则。
5. Skill invocation 如何触发，以及是否形成 Item。
6. SkillInjection、UserMessageCompositionInput 和 committed contribution stamp 的最终格式。
7. source watcher、显式 reload 和 debounce 行为。
8. cache 容量、eviction 和失败重试策略。
9. SkillService 初始化失败对 Runtime 启动的影响。
10. Session reload 后如何重新获得同一个 SkillService。

## 设计进度

- [x] 确定 MiniCoreRuntime 初始化并拥有一个 `Arc<SkillService>`。
- [x] 确定 durable Session 不持有 SkillService，Runtime 注入 loaded execution。
- [x] 确定 Turn 对象不持有 Skill、Catalog、LoadedSkill 或 Skill snapshot。
- [x] 确定 Catalog 只包含轻量 metadata，不包含完整正文。
- [x] 确定完整 Skill 内容由 SkillService 按需加载、解析和缓存。
- [x] 确定 SkillService 管理 discovery、filtering、cache、invalidation 和 diagnostics。
- [x] 确定 Turn execution 决定本次执行使用哪个 Skill。
- [x] 确定 TurnExecutionContext pin SkillCatalogContext、SkillCatalog 和 fingerprint。
- [x] 确定 SkillInjector 只负责 LoadedSkill 到 PromptContribution 的转换。
- [x] 确定用户侧Skill contribution必须固化到User/Steer entry；Skill Tool输出通过tool message + `tool_round_completed`进入conversation。
- [x] 确定 cache 失效不修改已经返回的不可变 LoadedSkill。
- [ ] 定义 SkillMetadata 和 SkillContent 的最终字段。
- [x] 定义 SkillCatalogContext 的 Session/Agent identity 与 WorkspaceSkillContext 输入。
- [x] 确定 load 使用 pinned SkillCatalogEntryRef，不重新解析 current Catalog。
- [x] 确定 Workspace Skill source adapter 在 discover/read 时强制校验 pinned context、lease 和 source stamp。
- [ ] 定义 SkillScope。
- [ ] 定义 SkillName 冲突和 namespace 规则。
- [ ] 定义 invocation、Item、UserMessageCompositionInput 和 PromptContribution stamp 的最终形状。
- [ ] 定义 watcher、reload、cache eviction 和失败重试策略。
