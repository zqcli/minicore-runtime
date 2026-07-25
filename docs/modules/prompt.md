# Prompt 子系统架构设计

状态：当前权威架构（设计已冻结，实现进行中）
日期：2026-07-16

## 目的

本文定义 MiniCore Prompt 子系统的基础对象、所有权、共享资源 view、Turn 快照、用户输入规范化、模型上下文组装、校验、fingerprint 和 diagnostics。

本文以以下关系为基础：

```text
MiniCoreRuntime 初始化一个 Arc<PromptService>
PromptService 发布共享、不可变的 PromptResourceView
AgentDefinition 保存默认 PromptId selection
SessionDefinition 保存本 Session 的 PromptId selection
Turn 领域对象不持有 PromptSet 或完整 Prompt 内容
Session execution 在 admission 期间捕获 current PromptResourceView
PromptService::for_turn(...) 为 candidate Turn 构造独立、不可变的 PromptSet
PromptSet 是唯一可以组装模型可见上下文的对象
```

本文不定义以下内容：

- Prompt 配置的持久化格式；
- Workspace、Agent 和 Session Prompt source 的具体文件格式；
- Prompt template 的最终语法；
- provider-specific role、cache-control 和 payload encoding；
- Compaction的cut、trigger和SessionExecutor orchestration；本文只固定CompactionSummary assembly contract；
- Prompt fingerprint/content reference 的 exact cold recovery；
- Prompt hook、远程 Prompt source 或插件协议的具体实现；
- PromptSet fingerprint 的持久化和审计格式。

## 决策摘要

本子系统的核心设计决策：

- `MiniCoreRuntime` 初始化并拥有一个 `Arc<PromptService>`；
- PromptService 是长生命周期深模块；
- PromptService 通过不可变 `PromptResourceView` 共享 Prompt definitions；
- AgentDefinition 和 SessionDefinition 只保存 `PromptId` selection，不复制 Prompt 正文；
- Turn 领域对象不持有 PromptSet，也不保存完整 Prompt definitions；
- Session execution 在领域 Turn 发布前、initiating UserMessage 规范化和第一次模型调用前创建 PromptSet；
- PromptSet 是某个 Turn 使用的不可变有效 Prompt 快照；
- Prompt role 只保留 `System` 与 `User`；
- Runtime required policy 不进入 selection，不能被 Agent 或 Session关闭；
- PromptService 可以加载 Prompt-specific source，但不拥有 Workspace 生命周期或 trust 状态；
- PromptService 不主动调用 ToolService 或 SkillService；
- Session execution先取得`PromptResourceView`、`ToolPromptView`和`SkillPromptView`，再交给PromptService；
- PromptSet 负责 `PromptIntent → CanonicalUserMessage`；
- PromptSet 负责每次模型调用的最终 provider-neutral context assembly；
- PromptSet在创建时绑定这些view，assembly时不再接受任意替代view；
- 执行中变化的模型可见事实必须先进入 committed conversation，不保留 arbitrary current-call contribution lane；
- `MessageRecord → ModelMessage` 的唯一转换发生在 Prompt 子系统；
- 相同输入必须产生相同排序、输出和 fingerprint；
- PromptService 和 PromptSet 都不执行 Tool、不加载 Skill、不保存 conversation，也不调用模型。

## 对象关系

```text
MiniCoreRuntime
└─ Arc<PromptService>
   ├─ current Arc<PromptResourceView>
   ├─ PromptSourceAdapter*
   ├─ PromptContentCache
   ├─ PromptPolicy
   └─ PromptDiagnostics

AgentDefinition
└─ AgentPromptSelection

SessionDefinition
└─ SessionPromptSelection

Turn execution orchestration
├─ PromptService.current_view() → Arc<PromptResourceView>
├─ ToolSet.prompt_view()         → ToolPromptView
├─ SkillView.prompt_view()       → SkillPromptView
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

AgentDefinition 只保存共享 Prompt resource 的默认选择：

```rust
pub struct AgentDefinition {
    pub agent_id: AgentId,
    pub revision: AgentRevision,
    pub prompts: AgentPromptSelection,
    // ...
}

pub struct AgentPromptSelection {
    pub enabled: BTreeSet<PromptId>,
}
```

SessionDefinition保存本Session的User Prompt选择，由SessionDefinitionRevision独立管理：

```rust
pub struct SessionDefinition {
    pub session_id: SessionId,
    pub revision: SessionDefinitionRevision,
    pub agent: AgentRevisionRef,
    pub workspace: Workspace,
    pub model: SessionModelConfig,
    pub prompts: SessionPromptSelection,
}

pub struct SessionPromptSelection {
    pub enabled: BTreeSet<PromptId>,
}
```

多个 Session 可以选择同一个 `PromptId`。共享的是 PromptService view 中的不可变 `Arc<PromptDefinition>`，不是 PromptSet；每个 Turn 仍根据自己的 Session selection、Workspace、Tool、Skill和conversation构造独立 PromptSet。

Turn admission 从同一个 exact SessionDefinitionRevision 取得 selection，并按 `AgentRevisionRef` 读取 exact AgentDefinition。PromptService 不能按 AgentId 回查 current revision，也不能把不同 Session revision 的 Workspace、SessionModelConfig和Prompt selection 拼接。

Workspace project Prompt不写入Session selection。PromptService通过当前Turn pin的`WorkspacePromptContext`读取已授权project Prompt source，并将其作为User context加入本Turn PromptSet。Workspace只授权source，不解析Prompt。

Agent head、Session head 和 definitions 都不保存 PromptService、PromptContentCache、PromptSet 或最终 AssembledModelContext。完整 lifecycle 见 [Agent 与 Session 生命周期架构设计](agent-session-lifecycle.md)，Workspace 规则见 [Workspace 子系统架构设计](workspace.md)。

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
→ PromptSet、ToolSet、captured SkillView和WorkspaceSnapshot等执行期对象
```

Session execution 在 candidate admission 期间创建 PromptSet，并在 admission 失败或 Turn terminal 后随 Context 释放。PromptService 不创建 Turn，也不修改 TurnStatus。完整 capture、committed-only assembly 和 AgentLoop 关系见 [Turn 执行模块与执行上下文架构设计](turn-execution-context.md)。

## PromptService

PromptService对外隐藏Prompt source discovery、共享view publication、内容加载、cache、selection解析、排序、policy、校验和diagnostics：

```rust
pub struct PromptService {
    current: Arc<PromptResourceView>,
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

    pub fn current_view(&self) -> Arc<PromptResourceView>;

    pub async fn reload(&self) -> Result<Arc<PromptResourceView>, PromptError>;

    pub async fn for_turn(
        &self,
        context: PromptTurnContext,
    ) -> Result<PromptSet, PromptError>;
}
```

`initialize()`构建第一个`PromptResourceView`，不创建Agent、Session或Turn。`reload()`先完整构建并校验candidate view，成功后原子替换current view；失败时继续保留旧view。

`for_turn()`完成：

```text
PromptResourceView
+ AgentPromptSelection
+ SessionPromptSelection
+ WorkspacePromptContext
+ TurnModelSnapshot
+ ToolPromptView
+ SkillPromptView
→ 解析PromptId selection
→ required policy校验
→ 固定System/User分配
→ 稳定排序
→ PromptProfile
→ PromptSet
```

## Prompt Source Adapter

共享Prompt resource通过内部adapter接入：

```rust
pub trait PromptSourceAdapter: Send + Sync {
    async fn discover(&self) -> Result<Vec<PromptDefinition>, PromptSourceError>;
}
```

首版只需要Runtime built-in和用户配置source。AgentDefinition与SessionDefinition引用这些共享definition，不拥有独立source adapter。

Workspace project instructions不进入共享PromptResourceView。`for_turn()`使用同一个Turn-pinned `WorkspacePromptContext`读取已授权source并生成User context section。该context至少包含canonical cwd、primary root、已授权Prompt source roots、authorization lease和WorkspacePromptFingerprint；它不包含write capability，也不能从filesystem-readable additional roots自行扩大Prompt source。完整定义见[Workspace子系统架构设计](workspace.md)。

## PromptDefinition

```rust
pub struct PromptDefinition {
    pub id: PromptId,
    pub version: DefinitionVersion,
    pub key: PromptKey,
    pub name: String,
    pub description: Option<String>,
    pub role: PromptRole,
    pub content: PromptContent,
    pub provenance: PromptProvenance,
}

pub enum PromptProvenance {
    Runtime(PromptSourceId),
    User(PromptSourceId),
}
```

共享Prompt content cache key不能只使用PromptId；必须覆盖definition version和provenance。Workspace project instruction保留独立`WorkspaceSourceRef`，撤销对应Workspace lease后，active Turn不得再次使用该PromptSet发起模型调用。

```rust
pub enum PromptRole {
    System,
    User,
}
```

固定分配：

```text
Runtime required/base policy、Agent behavior
→ System

Session instructions、Workspace instructions、Skill metadata/正文、用户输入
→ User
```

Tool schema进入provider原生`tools`字段；ToolPromptView的说明性metadata进入User context。Runtime通用Tool安全规则若存在，属于Runtime System policy。低信任Workspace或Skill内容不能声明或提升为System。

所有普通selected Prompt按固定层级追加，不提供ReplaceBase或caller-controlled merge mode。Runtime required policy由PromptService单独持有并始终加入。

PromptDefinition的identity是`PromptId + DefinitionVersion`。selection变化不产生新DefinitionVersion。

## 共享资源 View

```rust
pub struct PromptResourceView {
    definitions: HashMap<PromptId, Arc<PromptDefinition>>,
    pub fingerprint: PromptResourceFingerprint,
}
```

PromptService是共享PromptDefinition的owner。Agent和Session只保存PromptId selection；同一definition可以被任意多个Session使用而不复制正文或加载状态。

selection只适用于普通可选Prompt。Runtime required policy始终由PromptService加入，不出现在selection中。Agent selection只能选择声明为System且来源可信的definition；Session selection只能选择User definition。role不匹配或PromptId不存在时返回typed error，不能静默忽略。

PromptResourceView发布后不可变。reload成功后只替换PromptService的current view；已经创建的PromptSet继续持有旧definition `Arc`，future Turn捕获新view。

## 外部窄 View

PromptService不接收ToolService、ToolSet、SkillService或完整SkillView handle，只接收模型安全的只读view。

```rust
pub struct ToolPromptView {
    pub specs: Arc<[ToolSpec]>,
    pub tool_set_fingerprint: ToolSetFingerprint,
}

pub struct SkillPromptView {
    pub entries: Arc<[ModelVisibleSkillMetadata]>,
    pub fingerprint: SkillViewFingerprint,
}
```

ToolPromptView 由当前 ToolSet 投影。它不能执行 Tool，也不暴露 executor、approval、policy、grant 或 Sandbox。

SkillPromptView由Turn捕获的SkillView投影。它不包含完整Skill正文，也不能通过PromptService加载Skill。

## PromptTurnContext

Turn 编排层交给 PromptService 的输入：

```rust
pub struct PromptTurnContext {
    pub agent: AgentRevisionRef,
    pub session_id: SessionId,
    pub session_revision: SessionDefinitionRevision,
    pub resources: Arc<PromptResourceView>,
    pub agent_prompts: AgentPromptSelection,
    pub session_prompts: SessionPromptSelection,
    pub workspace: WorkspacePromptContext,
    pub model: TurnModelSnapshot,
    pub tools: ToolPromptView,
    pub skills: SkillPromptView,
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
    skill_view_fingerprint: SkillViewFingerprint,
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
    pub user_context: Arc<[PromptSection]>,
}
```

PromptProfile保存已经按固定层级和稳定顺序解析完成的Prompt baseline。`system`只包含Runtime required/base policy与Agent behavior；`user_context`包含Session、Workspace、Tool说明性metadata和Skill metadata，并在AgentRun assembly时编码为位于committed conversation之前的确定性User context。每个PromptSection自带definition provenance/source stamp；它与committed MessageRecord中的PromptContributionStamp不是同一类identity。SkillPromptView metadata在创建PromptProfile时被稳定渲染，PromptSet另外保存其fingerprint用于一致性校验。

固定顺序：

```text
1. Runtime required system policy
2. Runtime base system Prompt
3. Agent system instructions
4. Session user instructions
5. Workspace user instructions
6. ToolPromptView user-facing guidelines metadata
7. SkillPromptView user metadata
```

该顺序参考Codex和Claude Code的固定层级：高信任层的位置不能被低层配置或文件扫描顺序改变。不定义`priority`字段。

各层使用与其数据类型一致的稳定顺序：

```text
Runtime/Agent/Session selected PromptDefinition
→ PromptKey → PromptId → DefinitionVersion → provenance source identity

Workspace instructions
→ model-safe relative path

ToolPromptView metadata
→ ToolName

SkillPromptView metadata
→ SkillId
```

source adapter返回顺序、filesystem枚举顺序和HashMap迭代顺序都不能影响结果。PromptDefinition层内出现相同`PromptKey`时返回`PromptErrorKind::DuplicateKey`并拒绝创建PromptSet；不静默覆盖或选择“最后发现”的definition。

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

SkillIntent的完整Skill内容必须先由TurnExecutionContext使用本Turn捕获的SkillView entry调用SkillService加载，并经SkillInjector转换为PromptContribution。PromptSet不读取Skill文件，只校验contribution来源并将其规范化进MessageRecord。

同样的规范化规则可以服务于 Steer control fact，但 storage/domain fact kind 决定它是否开启新 Turn；模型 role 不能反向决定领域 Turn 边界。

## PromptContribution

PromptContribution 表示在输入规范化前由其他深模块产生的 typed 模型材料。它不是每次模型调用都可以临时追加的旁路：

```rust
pub struct PromptContribution {
    pub source: PromptContributionSource,
    pub content: MessageContent,
}

pub enum PromptContributionSource {
    Skill(SkillContributionRef),
    Tool(ToolName),
    Workspace(WorkspaceSourceRef),
}
```

`WorkspaceSourceRef` 必须携带 root/source identity、model-safe relative path 和 source authorization stamp；不能使用裸绝对 `PathBuf` 表达已授权 Workspace contribution。

`SkillContributionRef`携带SkillId和source authorization stamp。TurnExecutionContext负责确认entry来自本Turn捕获的SkillView并在实际读取时重新校验Workspace authorization；PromptSet把来源stamp固化到CanonicalUserMessage，正文完整性由CanonicalUserMessage自身的fingerprint覆盖。

PromptContribution固定为User内容，不能声明System role。producer负责I/O、加载和错误分类；PromptSet只验证、排序并把它固化到`CanonicalUserMessage`或Steer user message。模型触发的Skill Tool输出走truthful role=tool message + `tool_round_completed`路径，不形成未归属的PromptContribution lane。

Required contribution 获取失败必须显式返回 unavailable/error，不能通过 vector 缺项静默忽略。

基础生命周期：

```text
PromptContribution
→ PromptSet 输入规范化
→ User MessageRecord + PromptContributionStamp
→ SessionWriter.append + apply_committed
→ conversation projection接纳
→ 后续assembly只从committed conversation重建
```

Turn-static Workspace Prompt、ToolPromptView和SkillView metadata在PromptSet创建时固定，不经过每次调用的 PromptContribution。未来若引入动态Context provider，其输出也必须先经过同一规范化与append/apply和conversation projection规则，不能恢复current-call assembly旁路。

## 模型上下文组装

每次模型调用的输入只包含 committed conversation proof 和 typed call policy：

```rust
pub enum PromptAssemblyInput<'a> {
    AgentRun {
        conversation: &'a CommittedConversationView,
        output_contract: Option<&'a OutputContract>,
    },
    CompactionSummary {
        source: &'a CommittedCompactionSourceView,
        directive: &'a CompactionSummaryDirective,
    },
}
```

variant确定`ModelCallPurpose`，caller不能把Compaction source伪装成AgentRun input。`CommittedConversationView`只能从已验证的`CommittedConversationState::view()`获得；`CommittedCompactionSourceView`只能由同一State按validated Compaction scope、anchor、previous checkpoint、coverage provenance和stable-unit boundaries构造。它把不进入coverage的protected context与真正待摘要的messages分开；active scope的protected context包含current effective prefix through selected exact initiating/Steer UserMessage anchor，不能为了让summary request fit而静默删减。State只能由SessionStorage replay构造，或在成功应用`SessionWriter::append()`返回的`CommittedSessionEntry` trusted delta后前进。其checkpoint/fingerprint和apply规则见[Conversation与SessionStorage架构设计](conversation-storage.md)，Compaction-specific规则见[Compaction架构设计](compaction.md)。PromptSet assembly不接收裸`Vec<MessageRecord>`、任意ToolPromptView或任意PromptContribution。

`CompactionSummary`固定`OutputContract::NoToolCalls`和empty ToolSpec，只组装Runtime required System policy、typed User summary directive和trusted scope-aware committed source。directive中的effective summary budget必须来自Compaction plan，并与pinned model fingerprint一起进入assembly proof；PromptSet不能重新clamp或扩大。普通Agent/Session/Workspace/Tool/Skill静态内容不进入摘要请求；下一次`AgentRun` assembly重新注入同一个Turn-pinned PromptSet内容。

planning前，SessionExecutor从同一个PromptSet取得窄的固定开销basis：

```rust
pub struct CompactionSummaryAssemblyBasis {
    pub fixed_prompt_tokens: u64,
    pub fingerprint: CompactionSummaryAssemblyBasisFingerprint,
}

impl PromptSet {
    pub(crate) fn compaction_summary_assembly_basis(
        &self,
    ) -> CompactionSummaryAssemblyBasis;
}
```

该basis只覆盖Runtime required summary System policy、`NoToolCalls` output contract和empty ToolSpec的固定组装开销；不包含conversation source、Compaction directive正文或任意动态contribution。Compaction负责把basis、candidate-specific directive/source estimate、pinned model limits和safety reserve合成为最终`CompactionSummaryBudget`。最终assembly必须复算并验证basis fingerprint与实际固定sections一致。

最终输出：

```rust
pub struct AssembledModelContext {
    pub system: Arc<[PromptSection]>,
    pub messages: Arc<[ModelMessage]>,
    pub tools: Arc<[ToolSpec]>,
    pub output_contract: Option<OutputContract>,
    pub contribution_stamps: Arc<[PromptContributionStamp]>,
    pub diagnostics: Arc<[PromptDiagnostic]>,
    pub fingerprint: AssembledModelContextFingerprint,
    pub(crate) assembly_proof: PromptAssemblyProof,
}

pub(crate) struct PromptAssemblyProof {
    pub purpose: ModelCallPurpose,
    pub turn_model_fingerprint: TurnModelFingerprint,
    pub output_contract_hash: Option<ContentHash>,
    pub compaction_summary_budget: Option<CompactionSummaryBudgetProof>,
}

pub(crate) struct CompactionSummaryBudgetProof {
    pub max_output_tokens: NonZeroU32,
    pub budget_fingerprint: CompactionSummaryBudgetFingerprint,
}
```

AssembledModelContext是唯一允许进入ModelGateway的provider-neutral Prompt输出。`system`只保存有序System section；Session/Workspace/Skill等User context已经确定性地位于`messages`前部。`assembly_proof`是crate-private consistency proof，不是第二个caller-controlled purpose；`ModelCallRequest::new(...)`用它校验purpose、TurnModelSnapshot、OutputContract binding，以及CompactionSummary request max output与budget fingerprint。AgentRun的`compaction_summary_budget = None`，CompactionSummary必须为`Some`。provider原生System字段、User message和cache-control encoding由[ModelGateway](model-gateway.md)处理。

`MessageRecord → ModelMessage` 的唯一转换发生在 `PromptSet::assemble()` 内。

## 最终校验

`PromptSet::assemble()` 集中执行：

- System section和前置User context顺序确定；
- required Runtime policy 未缺失；
- PromptKey 和 contribution source 不发生非法重复；
- PromptSet 内绑定的 ToolPromptView 携带 parent ToolSetFingerprint；该 cross-binding 在 TurnExecutionContext capture/final validation 时完成；
- 不存在 orphan ToolResult；
- 不存在非法截断的 unresolved ToolCall；
- initiating UserMessage 未遗漏或放到 ToolCall/ToolResult 中间；
- committed MessageRecord中的SkillContributionRef和source stamp与规范化时保存的stamp一致；
- required contribution 在输入规范化阶段缺失时失败；
- 不存在未append/apply或尚未进入conversation projection的current-call model-visible contribution；
- output contract 不被伪装成普通 Prompt text；
- CompactionSummary directive budget与assembly proof exact match，AgentRun不携带Compaction budget proof；
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
PromptResourceView fingerprint
selected PromptId集合
PromptDefinition identity/version
PromptDefinition provenance/source authorization stamp
System/User分配
WorkspacePromptFingerprint
ToolPromptView.tool_set_fingerprint
SkillPromptView fingerprint
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
├─ PromptService.current_view() → Arc<PromptResourceView>
├─ SkillService.current_view(context) → Arc<SkillView> → SkillPromptView
└─ ToolService.for_turn(pinned context) → ToolSet.prompt_view()

三个view均就绪
→ PromptService.for_turn(PromptTurnContext)
→ PromptSet

用户输入
→ TurnExecutionContext.compose_message(PromptIntent)
→ 内部按需load Skill / SkillInjector.build
→ PromptSet.compose_user_message(...)
→ CanonicalUserMessage
→ append initiating UserMessage entry

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

Claude Code使用managed、user、project、local和path-scoped instructions，并对Skill内容使用按需加载。MiniCore同样共享资源、按Session构建上下文；只保留System与User两种Prompt role。

## 错误和 Diagnostics

基础错误分类：

```rust
pub enum PromptErrorKind {
    SourceDiscovery,
    ContentLoad,
    DuplicateKey,
    PromptUnavailable,
    InvalidRole,
    RequiredPromptMissing,
    InvalidIntent,
    InvalidContribution,
    ToolConversationMismatch,
    ContextLimitExceeded,
}
```

PromptService保存source/load/reload diagnostics；PromptSet保存本Turn的selection和assembly diagnostics。

非致命source error可以保留有效Prompt definitions并产生diagnostics。DuplicateKey、Required policy、required contribution或conversation protocol错误必须fail closed。

## 领域所有权

| 对象 | Owner |
| --- | --- |
| `Arc<PromptService>` 生命周期 | MiniCoreRuntime |
| PromptResourceView、Prompt definitions/source/cache | PromptService |
| AgentPromptSelection | exact AgentDefinition revision |
| SessionPromptSelection / Workspace definition | exact SessionDefinition revision |
| Workspace project Prompt discovery、definition 和正文 cache | PromptService，经 WorkspacePromptContext 授权 |
| PromptSet | Turn 执行上下文 |
| ToolPromptView | ToolSet 投影 |
| SkillPromptView | SkillView 投影 |
| LoadedSkill → PromptContribution | SkillInjector；由TurnExecutionContext保证view来源和读取授权 |
| PromptContribution → committed MessageRecord | PromptSet 输入规范化 |
| conversation | Session conversation owner |
| CanonicalUserMessage / final context assembly | PromptSet |
| provider payload encoding | ModelGateway |

## 基础不变量

- 一个 MiniCoreRuntime 初始化一个 PromptService；
- PromptService拥有共享PromptResourceView；AgentDefinition和SessionDefinition只保存PromptId selection；
- 同一个PromptDefinition可以被多个Session选择，但每个Turn独立构造PromptSet；
- Prompt capture使用exact AgentRevisionRef、SessionDefinitionRevision和当时的current PromptResourceView，不读取Agent current；
- Turn 领域对象不持有完整 PromptSet；
- TurnExecutionContext 在本 Turn 内复用同一个不可变 PromptSet；
- PromptSet在创建时固定PromptResourceView、ToolPromptView、渲染后的SkillPromptView metadata和相应fingerprint；
- PromptService 不主动调用 ToolService、SkillService 或 ModelGateway；
- PromptSet 不执行 Tool、不加载 Skill、不读写 conversation storage；
- Runtime required policy不进入selection，不可被Agent或Session关闭；
- Prompt role只保留System和User；Runtime/Agent可信行为进入System，Session/Workspace/Skill进入User；
- Workspace file readable 不等于可作为 Prompt source；
- PromptService 只能从 WorkspacePromptContext 授权的 source roots 加载 project Prompt；
- Workspace project instruction必须保留typed WorkspaceSourceRef/source stamp，cache和fingerprint不能只按path复用；
- Prompt baseline使用固定信任层顺序；层内使用stable identity全序，不存在caller-controlled priority；
- 同一固定层内重复PromptKey fail closed；
- `MessageRecord → ModelMessage` 只有一个转换入口；
- assembly只接受来自CommittedConversationState的trusted full/prefix view和closed typed call policy，不接受任意Tool view或current-call contribution；
- AssembledModelContext 是进入 ModelGateway 的唯一 Prompt 输出；
- 相同输入产生相同排序和 fingerprint；
- Prompt reload先构建candidate view，成功后原子替换current view；不原地修改active PromptSet。

## 后续问题

1. PromptContent 是内联正文还是 immutable content reference。
2. PromptResourceView的内部publication和cache eviction实现。
3. Prompt template 是否属于 PromptDefinition kind，还是独立 helper。
4. SkillIntent、UserMessageCompositionInput 与 committed contribution stamp 的精确字段。
5. ToolPromptView 是否包含 guidelines，以及 ToolSpec 如何进入最终 Prompt。
6. PromptSet fingerprint 的序列化和 Turn recovery 规则。
7. Prompt content cache 的 key、eviction 和失效策略。
8. Prompt hook 和动态 Context provider 是否能在不建立未提交模型可见旁路的前提下接入。
9. Provider cache效果和canonical instruction boundary验证。
10. PromptError 与 Turn terminal/compaction 的映射。
