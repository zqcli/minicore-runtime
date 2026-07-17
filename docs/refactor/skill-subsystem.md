# Skill 子系统架构设计

状态：基础架构已确定；实现细节待补充
日期：2026-07-16

## 目的

本文定义 MiniCore Skill 子系统的基础对象、所有权、渐进披露、按需加载、缓存、失效和 Prompt Injection 关系。

本文以以下关系为基础：

```text
MiniCoreRuntime 初始化一个 Arc<SkillsService>
Session 持有该 Arc<SkillsService> 的引用
Turn 不持有 Skill、Skill Catalog、LoadedSkill 或 TurnSkills
Turn 执行期间通过 Session 的 SkillsService 获取或加载 Skill
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

- `SkillsService` 在 `MiniCoreRuntime` 启动时初始化；
- 一个 Runtime 内的 Session 共享同一个 `Arc<SkillsService>`；
- Session 不复制 Skill definitions、Skill references、Catalog entries 或 Skill 正文；
- SkillsService 负责 Skill 发现、metadata 解析、过滤、Catalog 构建、完整内容加载、解析、缓存、失效和 diagnostics；
- Skill Catalog 是轻量、可重建的 metadata snapshot；
- Catalog 不包含完整 Skill 内容；
- Skill 内容默认按需加载；
- Turn 对象不保存任何 Skill 字段；
- 哪一个 Turn 在执行期间需要哪个 Skill，由 Turn 编排层决定；
- Turn 编排层通过对应 Session 的 SkillsService 请求 Skill；
- `SkillInjector` 只负责把已经加载的 Skill 转换成 Prompt contribution；
- Prompt 负责把 Skill contribution 与其他输入组装成最终模型上下文；
- 已加载 Skill 内容是不可变值；
- cache 失效不会修改已经返回的 `Arc<LoadedSkill>`；
- Catalog 或内容失效影响后续 catalog/load 结果，不回写已经开始的模型调用。

## 对象关系

```text
MiniCoreRuntime
└─ Arc<SkillsService>
   ├─ SkillSourceAdapter*
   ├─ SkillCatalogCache
   ├─ SkillContentCache
   ├─ SkillLoadStateStore
   └─ SkillDiagnostics

Session
└─ Arc<SkillsService>   // clone Runtime 持有的同一个 Arc

Turn execution
└─ Session.skills
   ├─ catalog(...)
   └─ load(session context, skill_id)
      └─ Arc<LoadedSkill>
         └─ SkillInjector
            └─ PromptContribution
               └─ Prompt assembly
```

## MiniCoreRuntime

`MiniCoreRuntime` 是 SkillsService 的创建者和生命周期 owner：

```rust
pub struct MiniCoreRuntime {
    pub prompts: RuntimePrompts,
    pub tools: Arc<ToolRuntime>,
    pub skills: Arc<SkillsService>,
}
```

Runtime 启动时：

1. 创建 Skill source adapters；
2. 创建 `SkillsService`；
3. 初始化基础 discovery 和 metadata Catalog；
4. 将同一个 `Arc<SkillsService>` 提供给后续创建的 Session；
5. Runtime shutdown 时停止新的 discovery/load，并释放子系统资源。

## Session

Session 直接持有 Runtime 创建的 SkillsService 引用：

```rust
pub struct Session {
    pub id: SessionId,
    pub agent_id: AgentId,
    pub status: SessionStatus,
    pub name: Option<String>,
    pub description: Option<String>,
    pub workspace: Workspace,
    pub model: Model,
    pub prompts: SessionPrompts,
    pub skills: Arc<SkillsService>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}
```

Session 不保存：

```text
SessionSkills
SkillRef[]
SkillDefinition[]
SkillCatalog entries
LoadedSkill[]
SkillContent
Skill cache
```

Session 可以通过 SkillsService 建立或获取适合当前 Session 的 Catalog。Catalog 构建需要哪些 Session/Workspace 输入，留待 Workspace 和 Skill filtering 设计时确定。

## Turn

Turn 对象不持有 Skill：

```rust
pub struct Turn {
    pub id: TurnId,
    pub session_id: SessionId,
    pub status: TurnStatus,
    pub model: TurnModel,
    pub prompts: TurnPrompts,
    pub items: Vec<Item>,
    pub started_at: Timestamp,
    pub completed_at: Option<Timestamp>,
}
```

Turn 不包含：

```text
skills
TurnSkills
SkillCatalog
SkillCatalogRevision
SkillRef
SkillId[]
LoadedSkill[]
```

Turn 执行期间若确定需要某个 Skill，Turn 编排层通过 `Turn.session_id` 找到 Session，再使用 `Session.skills` 完成 metadata 查询或完整内容加载。加载结果直接进入 Injection 流程，不写回 Turn 对象。

## SkillsService

`SkillsService` 是 Skill 子系统的深模块，对外隐藏 discovery、文件读取、解析、过滤、cache、并发去重、失效和 diagnostics：

```rust
pub struct SkillsService {
    sources: Vec<Arc<dyn SkillSourceAdapter>>,
    catalog_cache: SkillCatalogCache,
    content_cache: SkillContentCache,
    load_states: SkillLoadStateStore,
    diagnostics: SkillDiagnostics,
}
```

基础 interface：

```rust
impl SkillsService {
    pub async fn initialize(&self) -> Result<(), SkillError>;

    pub async fn catalog(
        &self,
        context: SkillCatalogContext,
    ) -> Result<Arc<SkillCatalog>, SkillError>;

    pub async fn load(
        &self,
        context: SkillCatalogContext,
        skill_id: SkillId,
    ) -> Result<Arc<LoadedSkill>, SkillLoadError>;

    pub async fn invalidate(
        &self,
        request: SkillInvalidation,
    ) -> Result<(), SkillError>;
}
```

`catalog()` 只返回 metadata。`load()` 才允许读取和解析完整正文。由于 SkillsService 被多个 Session 共享，`load()` 必须携带与 Catalog 相同的 Session context，并重新校验目标 Skill 对该 Session 可用。

## Skill Source Adapter

不同物理来源通过内部 adapter 接入 SkillsService：

```rust
pub trait SkillSourceAdapter: Send + Sync {
    async fn discover(&self) -> Result<Vec<DiscoveredSkill>, SkillSourceError>;
    async fn read(&self, location: &SkillLocation) -> Result<Vec<u8>, SkillSourceError>;
}
```

本阶段只固定 source adapter seam，不定义 BuiltIn/User/Workspace/Agent/Session 等来源层级和优先级。

```rust
pub struct DiscoveredSkill {
    pub location: SkillLocation,
    pub modified_at: Option<Timestamp>,
    pub content_hash: Option<ContentHash>,
}
```

## Skill Catalog

Catalog 是渐进披露的第一阶段，只包含轻量 metadata：

```rust
pub struct SkillCatalog {
    pub revision: CatalogRevision,
    pub entries: Vec<SkillMetadata>,
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
```

`SkillScope` 在当前阶段只作为 metadata 和 filtering 输入，不定义层级、覆盖或优先级语义：

```rust
pub struct SkillScope;
```

Catalog 必须满足：

- 不包含 Skill 正文；
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

SkillsService 单独维护实际加载状态：

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
- 同一精确定义的并发 load 应合并为一次底层读取和解析；
- load 成功后 cache 保存不可变 `Arc<LoadedSkill>`；
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

- 同一个 key 返回同一个或等价的不可变 LoadedSkill；
- cache 不改变 SkillMetadata；
- cache eviction 不改变 Catalog；
- Catalog metadata 指向新 content hash 时，后续 load 使用新 key；
- 已经返回的 `Arc<LoadedSkill>` 不被原地修改。

## Injection

Injection 层只消费已经加载的 Skill：

```rust
pub struct SkillInjection {
    pub skill_id: SkillId,
    pub version: DefinitionVersion,
    pub contribution: PromptContribution,
}

pub struct SkillInjector;

impl SkillInjector {
    pub fn build(skill: &LoadedSkill) -> Result<SkillInjection, SkillInjectionError>;
}
```

职责划分：

```text
SkillsService
→ 找到并加载什么内容

Turn 编排层
→ 本轮需要哪个 Skill

SkillInjector
→ 如何把 LoadedSkill 转换为 Prompt contribution

Prompt
→ 如何把 contribution 与其他上下文组装成模型输入
```

`SkillInjector` 不执行 discovery、Catalog filtering、文件读取、cache lookup 或 Skill 选择。

## 渐进披露流程

```text
MiniCoreRuntime 启动
→ 创建 Arc<SkillsService>
→ SkillsService.initialize()
→ 发现并解析轻量 metadata

Session 创建
→ clone Runtime 的 Arc<SkillsService>
→ Session.skills.catalog(session context)
→ 得到 SkillCatalog

Turn 执行期间确定需要 Skill
→ 通过 Session.skills.load(session context, skill_id)
→ cache hit，或读取、解析并缓存完整内容
→ 得到 Arc<LoadedSkill>

Injection
→ SkillInjector.build(loaded_skill)
→ SkillInjection / PromptContribution
→ Prompt 组装本轮模型输入
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
- 已经生成的 PromptContribution 不被回写；
- 新的 catalog/load 请求必须看到失效后的结果；
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

SkillsService 保存结构化 diagnostics。Catalog 可以返回有效 entries 和非致命 diagnostics；请求加载某个失败 Skill 时返回对应 typed error。

## 领域所有权

| 对象 | Owner |
| --- | --- |
| `Arc<SkillsService>` 生命周期 | MiniCoreRuntime |
| `Arc<SkillsService>` 引用 | Session |
| Skill discovery/filter/cache/invalidation | SkillsService |
| Skill Catalog | SkillsService |
| Skill metadata | SkillsService / Catalog |
| 完整 Skill 内容 | SkillsService content cache |
| 某次执行选择哪个 Skill | Turn 编排层 |
| Skill 到 Prompt contribution 的转换 | SkillInjector |
| 最终模型上下文组装 | Prompt |

## 基础不变量

- 一个 Runtime 初始化一个 SkillsService；
- Session 引用 Runtime 创建的同一个 SkillsService；
- Turn 对象不持有 Skill；
- Catalog 不包含完整正文；
- 完整正文只能通过 SkillsService 按需加载；
- Turn 编排层不能直接读取 Skill 文件；
- SkillInjector 不能决定选择哪个 Skill；
- Prompt 不能执行 Skill discovery 或 load；
- SkillsService 不决定哪个 Turn 使用哪个 Skill；
- cache 和 load state 不进入领域对象；
- active 使用中的不可变内容不被 reload 原地修改。

## 后续问题

1. SkillMetadata 的完整字段和 `DefinitionVersion` 生成方式。
2. SkillCatalogContext 需要哪些 Session/Workspace 信息。
3. SkillScope 的精确定义。
4. SkillName 冲突、namespace 和稳定排序规则。
5. Skill invocation 如何触发，以及是否形成 Item。
6. SkillInjection 的 PromptContribution 格式。
7. source watcher、显式 reload 和 debounce 行为。
8. cache 容量、eviction 和失败重试策略。
9. SkillsService 初始化失败对 Runtime 启动的影响。
10. Session reload 后如何重新获得同一个 SkillsService。

## 设计进度

- [x] 确定 MiniCoreRuntime 初始化并拥有一个 `Arc<SkillsService>`。
- [x] 确定 Session 引用 Runtime 创建的同一个 SkillsService。
- [x] 确定 Turn 对象不持有 Skill、Catalog、LoadedSkill 或 Skill snapshot。
- [x] 确定 Catalog 只包含轻量 metadata，不包含完整正文。
- [x] 确定完整 Skill 内容由 SkillsService 按需加载、解析和缓存。
- [x] 确定 SkillsService 管理 discovery、filtering、cache、invalidation 和 diagnostics。
- [x] 确定 Turn 编排层决定本次执行使用哪个 Skill。
- [x] 确定 SkillInjector 只负责 LoadedSkill 到 PromptContribution 的转换。
- [x] 确定 cache 失效不修改已经返回的不可变 LoadedSkill。
- [ ] 定义 SkillMetadata 和 SkillContent 的最终字段。
- [ ] 定义 SkillCatalogContext 和 SkillScope。
- [ ] 定义 SkillName 冲突和 namespace 规则。
- [ ] 定义 invocation、Item 和 PromptContribution 的最终形状。
- [ ] 定义 watcher、reload、cache eviction 和失败重试策略。
