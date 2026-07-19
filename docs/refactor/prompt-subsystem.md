# Prompt 子系统架构设计

状态：基础架构已确定；实现细节待补充
日期：2026-07-16

## 目的

本文定义 MiniCore Prompt 子系统的基础对象、所有权、Prompt source、scope 解析、Turn 快照、用户输入规范化、模型上下文组装、校验、fingerprint 和 diagnostics。

本文以以下关系为基础：

```text
MiniCoreRuntime 初始化一个 Arc<PromptService>
AgentDefinition 持有 AgentPrompts 配置
SessionDefinition 持有 SessionPrompts 配置
Turn 领域对象不持有 PromptSet 或完整 Prompt 内容
Session execution 在 admission 期间预留 candidate Turn identity
PromptService::for_turn(...) 返回 candidate Turn 使用的不可变 PromptSet
PromptSet 是唯一可以组装模型可见上下文的对象
```

本阶段暂不设计：

- Prompt 配置的持久化格式；
- Workspace、Agent 和 Session Prompt source 的具体文件格式；
- Prompt template 的最终语法；
- provider-specific role、cache-control 和 payload encoding；
- conversation storage、compaction 和 Turn recovery；
- Prompt hook、远程 Prompt source 或插件协议的具体实现；
- PromptSet fingerprint 的持久化和审计格式。

## 决策摘要

已经确定：

- `MiniCoreRuntime` 初始化并拥有一个 `Arc<PromptService>`；
- PromptService 是长生命周期深模块；
- AgentDefinition 和 SessionDefinition 只持有对应 scope 的 Prompt definitions 和 overrides；
- Turn 领域对象不持有 PromptSet，也不保存完整 Prompt definitions；
- Session execution 在领域 Turn 发布前、initiating UserMessage 规范化和第一次模型调用前创建 PromptSet；
- PromptSet 是某个 Turn 使用的不可变有效 Prompt 快照；
- Runtime、Agent、Session 是 Prompt 的配置 scope；
- System、Developer、User 是模型消息 role；
- Prompt scope 与 Prompt role 是两个正交维度；
- Runtime required policy 不能被 Agent 或 Session 覆盖；
- PromptService 可以加载 Prompt-specific source，但不拥有 Workspace 生命周期或 trust 状态；
- PromptService 不主动调用 ToolService 或 SkillService；
- Session execution 先取得 `ToolPromptView` 和 `SkillCatalogView`，再把窄 view 交给 PromptService；
- PromptSet 负责 `PromptIntent → CanonicalUserMessage`；
- PromptSet 负责每次模型调用的最终 provider-neutral context assembly；
- PromptSet 在创建时绑定 ToolPromptView 和 SkillCatalogView，assembly 时不再接受任意替代 view；
- 执行中变化的模型可见事实必须先进入 committed conversation，不保留 arbitrary current-call contribution lane；
- `MessageRecord → ModelMessage` 的唯一转换发生在 Prompt 子系统；
- 相同输入必须产生相同排序、输出和 fingerprint；
- PromptService 和 PromptSet 都不执行 Tool、不加载 Skill、不保存 conversation，也不调用模型。

## 对象关系

```text
MiniCoreRuntime
└─ Arc<PromptService>
   ├─ RuntimePrompts
   ├─ PromptSourceAdapter*
   ├─ PromptContentCache
   ├─ PromptPolicy
   └─ PromptDiagnostics

AgentDefinition
└─ AgentPrompts

SessionDefinition
└─ SessionPrompts

Turn execution orchestration
├─ ToolSet.prompt_view()     → ToolPromptView
├─ SkillCatalog.prompt_view() → SkillCatalogView
└─ PromptService::for_turn(PromptTurnContext)
   └─ PromptSet
      ├─ compose_user_message(UserMessageCompositionInput)
      │  └─ CanonicalUserMessage
      └─ assemble(PromptAssemblyInput)
         └─ AssembledModelContext
            └─ ModelGateway
```

## MiniCoreRuntime

`MiniCoreRuntime` 是 PromptService 的创建者和生命周期 owner：

```rust
pub struct MiniCoreRuntime {
    pub prompt_service: Arc<PromptService>,
    pub tools: Arc<ToolService>,
    pub skills: Arc<SkillService>,
}
```

Runtime 启动时：

1. 创建 Prompt source adapters；
2. 创建 `PromptService`；
3. 加载 RuntimePrompt definitions 和 required policy；
4. 初始化 Prompt content cache、policy 和 diagnostics；
5. Runtime shutdown 时停止新的 source load，并释放 Prompt 子系统资源。

## AgentDefinition 和 SessionDefinition

Agent scope Prompt 属于 immutable AgentDefinition：

```rust
pub struct AgentDefinition {
    pub agent_id: AgentId,
    pub revision: AgentRevision,
    pub prompts: AgentPrompts,
    // ...
}
```

Session scope Prompt 属于 immutable SessionDefinition：

```rust
pub struct SessionDefinition {
    pub session_id: SessionId,
    pub revision: SessionDefinitionRevision,
    pub agent: AgentRevisionRef,
    pub workspace: Workspace,
    pub model: Model,
    pub prompts: SessionPrompts,
}
```

Turn admission 从同一个 exact SessionDefinitionRevision 取得 SessionPrompts，并按 `AgentRevisionRef` 读取 exact AgentDefinition。PromptService 不能按 AgentId 回查 current revision，也不能把不同 Session revision 的 Workspace、Model 和 Prompt config 拼接。

Workspace project Prompt 在 scope 解析上属于 Session，但 SessionDefinition 不复制从项目文件发现的 PromptDefinition 或正文。PromptService 通过当前 Turn pin 的 `WorkspacePromptContext` 发现、加载和解析已授权 project Prompt source。Workspace 只授权 source，不解析 Prompt。

Agent head、Session head 和 definitions 都不保存 PromptService、PromptContentCache、PromptSet 或最终 AssembledModelContext。完整 lifecycle 见 [Agent 与 Session 生命周期架构设计](agent-session-lifecycle.md)，Workspace 规则见 [Workspace 子系统架构设计](workspace-subsystem.md)。

## Turn

Turn 领域对象不持有 PromptSet：

```rust
pub struct Turn {
    pub id: TurnId,
    pub session_id: SessionId,
    pub started_at: Timestamp,
    pub status: TurnStatus,
}
```

完整 PromptSet 属于 Turn 执行上下文，而不是领域 Turn：

```text
Turn domain
→ identity、started_at、terminal-aware status

Turn start execution metadata
→ exact Prompt/Model/Workspace/Tool/Skill fingerprints and references

TurnExecutionContext
→ PromptSet、ToolSet、pinned SkillCatalog 和 WorkspaceSnapshot 等执行期对象
```

Session execution 在 candidate admission 期间创建 PromptSet，并在 admission 失败或 Turn terminal 后随 Context 释放。PromptService 不创建 Turn，也不修改 TurnStatus。完整 capture、committed-only assembly 和 AgentLoop 关系见 [Turn 执行模块与执行上下文架构设计](turn-execution-context.md)。

## PromptService

PromptService 对外隐藏 Prompt source discovery、内容加载、cache、scope 解析、排序、policy、校验和 diagnostics：

```rust
pub struct PromptService {
    runtime_prompts: RuntimePrompts,
    sources: Vec<Arc<dyn PromptSourceAdapter>>,
    content_cache: PromptContentCache,
    policy: PromptPolicy,
    diagnostics: PromptDiagnostics,
}
```

基础 interface：

```rust
impl PromptService {
    pub async fn initialize(&self) -> Result<(), PromptError>;

    pub async fn for_turn(
        &self,
        context: PromptTurnContext,
    ) -> Result<PromptSet, PromptError>;
}
```

`initialize()` 只初始化 Runtime scope Prompt 和 Prompt source，不创建 Agent、Session 或 Turn。

`for_turn()` 完成：

```text
RuntimePrompts
+ AgentPrompts
+ SessionPrompts
+ WorkspacePromptContext
+ TurnModelSnapshot
+ ToolPromptView
+ SkillCatalogView
→ source load
→ scope defaults/overrides 解析
→ required policy 校验
→ role 和 merge policy 解析
→ 稳定排序
→ PromptProfile
→ PromptSet
```

## Prompt Source Adapter

Prompt-specific source 通过内部 adapter 接入：

```rust
pub trait PromptSourceAdapter: Send + Sync {
    async fn discover(
        &self,
        context: &PromptSourceContext,
    ) -> Result<Vec<PromptDefinition>, PromptSourceError>;
}
```

可能的 adapter：

```text
RuntimePromptSource
UserPromptSource
AgentPromptSource
SessionPromptSource
WorkspacePromptSource
```

PromptSourceAdapter 只读取 Prompt-specific source。它不创建 Workspace、不决定 Workspace trust，也不获得 provider credentials。Agent/Session source adapter 必须使用 context 中的 exact revisions，不能回查 mutable current head。

```rust
pub struct PromptSourceContext {
    pub agent: AgentRevisionRef,
    pub session_id: SessionId,
    pub session_revision: SessionDefinitionRevision,
    pub workspace: WorkspacePromptContext,
}
```

`WorkspacePromptContext` 由同一个 Turn-pinned `WorkspaceSnapshot` 投影，至少包含 canonical cwd、primary root、已授权 Prompt source roots、authorization lease 和 WorkspacePromptFingerprint。它不包含 write capability，也不能从 filesystem-readable additional roots 自行扩大 Prompt source。完整定义见 [Workspace 子系统架构设计](workspace-subsystem.md)。

## PromptDefinition

```rust
pub struct PromptDefinition {
    pub id: PromptId,
    pub version: DefinitionVersion,
    pub key: PromptKey,
    pub name: String,
    pub description: Option<String>,
    pub scope: PromptScope,
    pub role: PromptRole,
    pub merge: PromptMergeMode,
    pub content: PromptContent,
    pub provenance: PromptProvenance,
}

pub enum PromptProvenance {
    Runtime(PromptSourceId),
    Agent(PromptSourceId),
    Session(PromptSourceId),
    Workspace(WorkspaceSourceRef),
}
```

Workspace project Prompt 必须保留 `WorkspaceSourceRef`，其中包含 model-safe relative path、source authorization stamp 和 WorkspacePromptFingerprint。Prompt content cache key 不能只使用 path 或 PromptId；authorization-sensitive lookup 必须覆盖 provenance/source stamp。撤销对应 Workspace lease 后，active Turn 不得再次使用该 PromptSet 发起模型调用。

Prompt 的 scope 和 role 分开表达：

```rust
pub enum PromptScope {
    Runtime,
    Agent,
    Session,
}

pub enum PromptRole {
    System,
    Developer,
    User,
}
```

示例：

```text
Runtime required policy
→ scope = Runtime
→ role = System

Agent behavior instructions
→ scope = Agent
→ role = Developer

Workspace instructions
→ scope = Session
→ role = Developer

Prompt template invocation
→ scope = Session
→ role = User
```

Merge mode：

```rust
pub enum PromptMergeMode {
    Required,
    ReplaceBase,
    Append,
}
```

基础语义：

| Merge mode | 含义 |
| --- | --- |
| `Required` | 必须进入 PromptSet，低层 scope 不可删除或替换。 |
| `ReplaceBase` | 在 policy 允许时替换可替换的 base Prompt。 |
| `Append` | 按 scope、priority 和稳定 key 顺序追加。 |

PromptDefinition 的精确 identity 是 `PromptId + DefinitionVersion`。加载状态、启用状态和可见性变化不产生新 DefinitionVersion。

## Scope 集合

```rust
pub struct RuntimePrompts {
    pub definitions: Vec<PromptDefinition>,
    pub defaults: HashMap<PromptId, DefinitionOverrides>,
}

pub struct AgentPrompts {
    pub definitions: Vec<PromptDefinition>,
    pub overrides: HashMap<PromptId, DefinitionOverrides>,
}

pub struct SessionPrompts {
    pub definitions: Vec<PromptDefinition>,
    pub overrides: HashMap<PromptId, DefinitionOverrides>,
}
```

`Vec` 是 Prompt definitions 的权威有序集合。`PromptKey` 在同一 scope 内必须唯一。

```rust
pub struct DefinitionOverrides {
    pub enabled: Option<bool>,
    pub user_visible: Option<bool>,
    pub model_visible: Option<bool>,
    pub load_policy: Option<LoadPolicy>,
}
```

解析方向：

```text
Runtime defaults
+ Agent overrides
+ Session overrides
→ effective Prompt definitions
```

上层 Required 或明确禁止不能被低层 scope 解除。

## 外部窄 View

PromptService 不接收 ToolService、ToolSet、SkillService 或完整 SkillCatalog handle，只接收模型安全的只读 view。

```rust
pub struct ToolPromptView {
    pub specs: Arc<[ToolSpec]>,
    pub tool_set_fingerprint: ToolSetFingerprint,
}

pub struct SkillCatalogView {
    pub entries: Arc<[ModelVisibleSkillMetadata]>,
    pub revision: CatalogRevision,
    pub fingerprint: SkillCatalogFingerprint,
}
```

ToolPromptView 由当前 ToolSet 投影。它不能执行 Tool，也不暴露 executor、approval、policy、grant 或 Sandbox。

SkillCatalogView 由当前 SkillCatalog 投影。它不包含完整 Skill 正文，也不能通过 PromptService 加载 Skill。

## PromptTurnContext

Turn 编排层交给 PromptService 的输入：

```rust
pub struct PromptTurnContext {
    pub agent: AgentRevisionRef,
    pub session_id: SessionId,
    pub session_revision: SessionDefinitionRevision,
    pub agent_prompts: AgentPrompts,
    pub session_prompts: SessionPrompts,
    pub workspace: WorkspacePromptContext,
    pub model: TurnModelSnapshot,
    pub tools: ToolPromptView,
    pub skills: SkillCatalogView,
}
```

PromptTurnContext 不包含：

```text
Session storage
conversation
Tool executor
ToolSet handle
SkillService handle
LoadedSkill
provider client
credentials
Turn mutable state
```

## PromptSet

PromptSet 是某个 Turn 的不可变有效 Prompt 快照：

```rust
pub struct PromptSet {
    profile: PromptProfile,
    definitions: Arc<[EffectivePromptDefinition]>,
    tools: ToolPromptView,
    skill_catalog_fingerprint: SkillCatalogFingerprint,
    model: TurnModelSnapshot,
    fingerprint: PromptFingerprint,
}
```

```rust
impl PromptSet {
    pub fn compose_user_message(
        &self,
        input: UserMessageCompositionInput,
    ) -> Result<CanonicalUserMessage, PromptError>;

    pub fn assemble(
        &self,
        input: PromptAssemblyInput<'_>,
    ) -> Result<AssembledModelContext, PromptError>;
}
```

PromptSet 在同一个 Turn 中不原地修改。Prompt source reload 只影响 future PromptSet。

## PromptProfile

```rust
pub struct PromptProfile {
    pub system: Arc<[PromptSection]>,
    pub developer: Arc<[PromptSection]>,
}
```

PromptProfile 保存已经按 scope、role、merge policy 和稳定顺序解析完成的 Prompt baseline。每个 PromptSection 自带 definition provenance/source stamp；它与 committed MessageRecord 中的 PromptContributionStamp 不是同一类 identity。SkillCatalogView metadata 在创建 PromptProfile 时被稳定渲染，PromptSet 另外保存其 fingerprint 用于一致性校验。

推荐顺序：

```text
1. Runtime required system policy
2. Runtime base system Prompt
3. Agent instructions
4. Session instructions
5. Workspace instructions
6. ToolPromptView guidelines/spec metadata
7. SkillCatalogView metadata
```

同 scope 冲突产生 typed diagnostic，不能依赖 source discovery 顺序。

## PromptIntent 和 CanonicalUserMessage

所有 prompt-producing 输入先归一为 PromptIntent：

```rust
pub enum PromptIntent {
    Text(TextIntent),
    Template(PromptTemplateIntent),
    Skill(SkillIntent),
    Composite(CompositePromptIntent),
}
```

PromptSet 把 PromptIntent 和已经解析的 typed contribution 规范化成唯一用户消息：

```rust
pub struct UserMessageCompositionInput {
    pub intent: PromptIntent,
    pub contributions: Arc<[PromptContribution]>,
}

pub struct CanonicalUserMessage {
    pub message: MessageRecord,
    pub contribution_stamps: Arc<[PromptContributionStamp]>,
    pub fingerprint: CanonicalUserMessageFingerprint,
}
```

CanonicalUserMessage 是可以进入 conversation commit 的标准值，不是裸字符串，也不是与 MessageRecord 并列的第二份消息状态。

SkillIntent 的完整 Skill 内容必须先由 TurnExecutionContext 通过 pinned `SkillCatalogEntryRef` 调用 SkillService 加载，并经 SkillInjector 转换为 PromptContribution。PromptSet 不读取 Skill 文件，只校验 contribution identity 并将其规范化进 MessageRecord。

同样的规范化规则可以服务于 Steer control fact，但 storage/domain fact kind 决定它是否开启新 Turn；模型 role 不能反向决定领域 Turn 边界。

## PromptContribution

PromptContribution 表示在输入规范化前由其他深模块产生的 typed 模型材料。它不是每次模型调用都可以临时追加的旁路：

```rust
pub struct PromptContribution {
    pub source: PromptContributionSource,
    pub role: PromptRole,
    pub content: MessageContent,
    pub content_hash: ContentHash,
}

pub enum PromptContributionSource {
    Skill(SkillContributionRef),
    Tool(ToolName),
    Workspace(WorkspaceSourceRef),
}
```

`WorkspaceSourceRef` 必须携带 root/source identity、model-safe relative path 和 source authorization stamp；不能使用裸绝对 `PathBuf` 表达已授权 Workspace contribution。

`SkillContributionRef` 必须携带 Catalog revision、SkillId、DefinitionVersion、ContentHash 和 source authorization stamp。TurnExecutionContext 负责 pinned Catalog entry 与 LoadedSkill 的 exact-reference 校验；PromptSet 校验 contribution stamp 和 content hash，并将其固化到 CanonicalUserMessage fingerprint。

PromptContribution 的 producer 负责 I/O、加载和错误分类；PromptSet 只验证、排序并把它固化到 `CanonicalUserMessage`、Steer record 或其他 committed MessageRecord。

Required contribution 获取失败必须显式返回 unavailable/error，不能通过 vector 缺项静默忽略。

基础生命周期：

```text
PromptContribution
→ PromptSet 输入规范化
→ MessageRecord + PromptContributionStamp
→ commit
→ 后续 assembly 只从 committed conversation 重建
```

Turn-static Workspace Prompt、ToolPromptView 和 SkillCatalog metadata 在 PromptSet 创建时固定，不经过每次调用的 PromptContribution。未来若引入动态 Context provider，其输出也必须先经过同一规范化与 commit gate，不能恢复 current-call assembly 旁路。

## 模型上下文组装

每次模型调用的输入只包含 committed conversation proof 和 typed call policy：

```rust
pub struct PromptAssemblyInput<'a> {
    pub conversation: &'a CommittedConversationView,
    pub output_contract: Option<&'a OutputContract>,
    pub purpose: ModelCallPurpose,
}
```

`CommittedConversationView` 只能由成功 commit 返回的 delta 或 SessionStorage recovery 构造。PromptSet assembly 不接收裸 `Vec<MessageRecord>`、任意 ToolPromptView 或任意 PromptContribution。

最终输出：

```rust
pub struct AssembledModelContext {
    pub system_prompt: Arc<str>,
    pub messages: Arc<[ModelMessage]>,
    pub tools: Arc<[ToolSpec]>,
    pub output_contract: Option<OutputContract>,
    pub contribution_stamps: Arc<[PromptContributionStamp]>,
    pub diagnostics: Arc<[PromptDiagnostic]>,
    pub fingerprint: AssembledModelContextFingerprint,
}
```

AssembledModelContext 是唯一允许进入 ModelGateway 的 provider-neutral Prompt 输出。

`MessageRecord → ModelMessage` 的唯一转换发生在 `PromptSet::assemble()` 内。

## 最终校验

`PromptSet::assemble()` 集中执行：

- system/developer section 顺序确定；
- required Runtime policy 未缺失；
- PromptKey 和 contribution source 不发生非法重复；
- PromptSet 内绑定的 ToolPromptView 携带 parent ToolSetFingerprint；该 cross-binding 在 TurnExecutionContext capture/final validation 时完成；
- 不存在 orphan ToolResult；
- 不存在非法截断的 unresolved ToolCall；
- initiating UserMessage 未遗漏或放到 ToolCall/ToolResult 中间；
- committed MessageRecord 中的 SkillContributionRef、content hash 和 source stamp 与规范化时保存的 stamp 一致；
- required contribution 在输入规范化阶段缺失时失败；
- 不存在未 commit 的 current-call model-visible contribution；
- output contract 不被伪装成普通 Prompt text；
- 最终大小和 token estimate 不超过有效模型限制。

PromptSet 不自行执行 compaction。超限时返回结构化 PromptError，由 Session execution 决定后续行为。

## Fingerprint

至少需要：

```text
PromptFingerprint
CanonicalUserMessageFingerprint
AssembledModelContextFingerprint
```

PromptSet fingerprint 至少覆盖：

```text
PromptDefinition identity/version
PromptDefinition provenance/source authorization stamp
scope resolution result
role 和 merge mode
WorkspacePromptFingerprint
ToolPromptView.tool_set_fingerprint
SkillCatalogView fingerprint
Model capability projection
稳定 section 顺序
```

AssembledModelContext fingerprint 另外覆盖 committed conversation fingerprint、output contract 和 model-call purpose。PromptContribution 已固化在 committed MessageRecord 及其 stamp 中，不作为独立 current-call 输入重复计算。

Fingerprint 用于一致性校验、diagnostics、测试和未来 provider cache，不代替 secret redaction。

## 调用流程

```text
MiniCoreRuntime 启动
→ 创建 Arc<PromptService>
→ PromptService.initialize()

candidate Turn admission
├─ SkillService.catalog(pinned context) → SkillCatalog.prompt_view()
└─ ToolService.for_turn(pinned context) → ToolSet.prompt_view()

两个 view 均就绪
→ PromptService.for_turn(PromptTurnContext)
→ PromptSet

用户输入
→ TurnExecutionContext.compose_message(PromptIntent)
→ 内部按需 exact-load Skill / SkillInjector.build
→ PromptSet.compose_user_message(...)
→ CanonicalUserMessage
→ start batch commit

每次模型调用
→ CommittedConversationView
→ PromptSet.assemble(...)
→ AssembledModelContext
→ ModelGateway
```

PromptService 不主动调用 ToolService、SkillService 或 ModelGateway。

## 与同类项目的关系

Codex 每次模型调用构造一个最终 `Prompt`，其中包含 conversation input、ToolSpec、base instructions 和 output schema。Skill/plugin injection 先转成 input item，再进入最终 Prompt。MiniCore 保留这种“每次调用形成唯一最终 Prompt”的思路，但把分散在 Turn 流程中的组装规则收敛到 PromptService/PromptSet。

pi 使用纯 `buildSystemPrompt(options)` 组装 custom/base prompt、tools、context files、skills、date 和 cwd。MiniCore 保留其纯确定性组装思路，但使用不可变 PromptSet、typed contribution 和 fingerprint，避免 active tool/resource 变化导致局部重建分裂。

Claude Code 使用 managed、user、project、local 和 path-scoped instructions，并对 Skill 内容使用按需加载。MiniCore 借鉴其 scope 分层和渐进披露，但显式区分 PromptScope 与 PromptRole。

## 错误和 Diagnostics

基础错误分类：

```rust
pub enum PromptErrorKind {
    SourceDiscovery,
    ContentLoad,
    DuplicateKey,
    InvalidMerge,
    RequiredPromptMissing,
    InvalidIntent,
    InvalidContribution,
    ToolConversationMismatch,
    ContextLimitExceeded,
}
```

PromptService 保存 source/load/scope diagnostics；PromptSet 保存本 Turn 的 resolution 和 assembly diagnostics。

非致命 source error 可以保留有效 Prompt definitions 并产生 diagnostics。Required policy、required contribution 或 conversation protocol 错误必须 fail closed。

## 领域所有权

| 对象 | Owner |
| --- | --- |
| `Arc<PromptService>` 生命周期 | MiniCoreRuntime |
| Runtime Prompt definitions/source/cache | PromptService |
| AgentPrompts 配置 | exact AgentDefinition revision |
| SessionPrompts / Workspace definition | exact SessionDefinition revision |
| Workspace project Prompt discovery、definition 和正文 cache | PromptService，经 WorkspacePromptContext 授权 |
| PromptSet | Turn 执行上下文 |
| ToolPromptView | ToolSet 投影 |
| SkillCatalogView | SkillCatalog 投影 |
| LoadedSkill → PromptContribution | SkillInjector；由 TurnExecutionContext 保证 pinned identity |
| PromptContribution → committed MessageRecord | PromptSet 输入规范化 |
| conversation | Session conversation owner |
| CanonicalUserMessage / final context assembly | PromptSet |
| provider payload encoding | ModelGateway |

## 基础不变量

- 一个 MiniCoreRuntime 初始化一个 PromptService；
- AgentDefinition 和 SessionDefinition 只保存自己 scope 的 Prompt 配置；
- Prompt capture 使用 exact AgentRevisionRef 和 SessionDefinitionRevision，不读取 Agent current；
- Turn 领域对象不持有完整 PromptSet；
- TurnExecutionContext 在本 Turn 内复用同一个不可变 PromptSet；
- PromptSet 在创建时固定 ToolPromptView、渲染后的 SkillCatalogView metadata 和 SkillCatalogFingerprint；
- PromptService 不主动调用 ToolService、SkillService 或 ModelGateway；
- PromptSet 不执行 Tool、不加载 Skill、不读写 conversation storage；
- Runtime required policy 不可被 Agent 或 Session 覆盖；
- Workspace Prompt 属于 Session scope；
- Workspace file readable 不等于可作为 Prompt source；
- PromptService 只能从 WorkspacePromptContext 授权的 source roots 加载 project Prompt；
- Workspace project PromptDefinition 必须保留 typed provenance/source stamp，cache 和 fingerprint 不能只按 path 或 PromptId 复用；
- PromptScope 与 PromptRole 分开；
- `MessageRecord → ModelMessage` 只有一个转换入口；
- assembly 只接受 CommittedConversationView，不接受任意 Tool view 或 current-call contribution；
- AssembledModelContext 是进入 ModelGateway 的唯一 Prompt 输出；
- 相同输入产生相同排序和 fingerprint；
- reload 不原地修改 active PromptSet。

## 后续问题

1. PromptDefinition priority 和同 scope 冲突规则。
2. PromptContent 是内联正文还是 immutable content reference。
3. 多个 authorized Workspace Prompt roots 的 discovery precedence 和同 scope 冲突规则。
4. PromptSourceAdapter 的 BuiltIn/User/Agent/Session/Workspace 实现。
5. Prompt template 是否属于 PromptDefinition kind，还是独立 helper。
6. SkillIntent、UserMessageCompositionInput 与 committed contribution stamp 的精确字段。
7. ToolPromptView 是否包含 guidelines，以及 ToolSpec 如何进入最终 Prompt。
8. PromptSet fingerprint 的序列化和 Turn recovery 规则。
9. Prompt content cache 的 key、eviction 和失效策略。
10. Prompt hook 和动态 Context provider 是否能在不建立未提交模型可见旁路的前提下接入。
11. Provider 不支持 Developer role 时的 ModelGateway 映射规则。
12. PromptError 与 Turn terminal/compaction 的映射。

## 设计进度

- [x] 确定 MiniCoreRuntime 初始化并拥有一个 `Arc<PromptService>`。
- [x] 确定 AgentDefinition 和 SessionDefinition 持有 scope Prompt 配置。
- [x] 确定 Turn 领域对象不持有完整 PromptSet。
- [x] 确定 PromptService::for_turn 返回不可变 PromptSet。
- [x] 区分 PromptScope 和 PromptRole。
- [x] 确定 Runtime required policy 不可被低层 scope 覆盖。
- [x] 确定 PromptService 只消费 ToolPromptView 和 SkillCatalogView。
- [x] 确定 PromptSet 负责 CanonicalUserMessage 和最终模型上下文组装。
- [x] 确定 PromptSet 创建时绑定 ToolPromptView 和 SkillCatalogView。
- [x] 确定 assembly 只接受 CommittedConversationView 和 typed call policy。
- [x] 确定 PromptContribution 必须先固化到 committed MessageRecord，不作为 current-call 旁路。
- [x] 确定 MessageRecord 到 ModelMessage 的唯一转换入口。
- [x] 确定 PromptService/PromptSet 不执行 Tool、不加载 Skill、不调用模型。
- [ ] 定义 PromptDefinition priority、content identity 和 source adapters。
- [x] 定义 WorkspacePromptContext 的 owner、最小授权语义和 fingerprint；完整字段见 Workspace 子系统。
- [ ] 定义 PromptIntent、UserMessageCompositionInput、PromptContribution stamp 和 output contract 的最终字段。
- [ ] 定义 fingerprint、cache、失效和 Turn recovery 规则。
