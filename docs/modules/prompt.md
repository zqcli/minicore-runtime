# Prompt 子系统架构设计

状态：当前权威架构（ADR 0134后，生产实现待启动）
日期：2026-07-31

## 目的

本文定义 MiniCore Prompt 子系统的基础对象、所有权、共享资源 view、Turn 快照、用户输入规范化、模型上下文组装、校验和 diagnostics。

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
- Historical PromptSet/rendered Prompt审计格式；MVP不执行exact same-Turn cold recovery，也不保存PromptContent resolver；
- Prompt hook、远程 Prompt source 或插件协议的具体实现。

## 决策摘要

本子系统的核心设计决策：

- `MiniCoreRuntime` 初始化并拥有一个 `Arc<PromptService>`；
- PromptService 是长生命周期深模块；
- PromptService 通过不可变 `PromptResourceView` 共享 Prompt definitions；
- Prompt source在candidate build期间完全materialize为immutable `PromptContent`，实现通过强`Arc`共享正文；
- path、URL、source ID和cache key只用于adapter discovery/provenance/优化，不形成可重新解析或durable的`PromptContentRef`；
- AgentDefinition 和 SessionDefinition 只保存 `PromptId` selection，不复制 Prompt 正文；
- Turn 领域对象不持有 PromptSet，也不保存完整 Prompt definitions；
- Session execution 在领域 Turn 发布前、initiating UserMessage 规范化和第一次模型调用前创建 PromptSet；
- PromptSet 是某个 Turn 使用的不可变有效 Prompt 快照；
- Prompt role 只保留 `System` 与 `User`；
- Runtime required policy 不进入 selection，不能被 Agent 或 Session关闭；
- PromptService 可以加载 Prompt-specific source，但不拥有 Workspace 生命周期或 trust 状态；
- PromptService 不主动调用 ToolService 或 SkillService；
- Session execution先取得`PromptResourceView`、`ToolPromptView`和`SkillPromptView`，再交给PromptService；
- TurnExecutionContext异步解析captured Skill/Workspace contributions，PromptSet同步负责`UserMessageCompositionInput → CanonicalUserMessage`；
- PromptSet 负责每次模型调用的最终 provider-neutral context assembly；
- PromptSet在创建时绑定这些view，assembly时不再接受任意替代view；
- 执行中变化的模型可见事实必须先进入sanitized live conversation，不保留arbitrary current-call contribution lane；
- `ModelMessage` 的构造与provider-facing projection只属于Prompt；PromptSet仍是最终context assembly seam，但构造不限定在`PromptSet::assemble()`内；
- 相同输入必须产生相同排序和输出；
- PromptService 和 PromptSet 都不执行 Tool、不加载 Skill、不保存 conversation，也不调用模型。

## 对象关系

```text
MiniCoreRuntime
├─ Arc<PromptService>
│  ├─ PromptSourceAdapter*
│  ├─ PromptContentCache
│  ├─ PromptPolicy
│  └─ PromptDiagnostics
└─ SharedResourceRoots.prompt: Arc<PromptResourceView>

AgentDefinition
└─ AgentPromptSelection

SessionDefinition
└─ SessionPromptSelection

Turn execution orchestration
├─ captured SharedResourceRoots.prompt → Arc<PromptResourceView>
├─ ToolSet.prompt_view()         → ToolPromptView
├─ SkillView.prompt_view()       → SkillPromptView
└─ PromptService::for_turn(PromptTurnContext)
   └─ Arc<PromptSet>

User message resolution
└─ await TurnExecutionContext.resolve_user_message(PromptIntent)
   └─ PromptSet.compose_user_message(UserMessageCompositionInput)
      └─ CanonicalUserMessage

Model call assembly
└─ PromptSet.assemble(PromptAssemblyInput)
   └─ AssembledModelContext
      └─ ModelGateway
```

## MiniCoreRuntime

`MiniCoreRuntime` 是 PromptService 的创建者和生命周期 owner：

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

1. 创建 Prompt source adapters；
2. 创建 `PromptService`；
3. `PromptService::initialize()`构建initial PromptResourceView；
4. 四个module initial candidates全部成功后，Runtime安装SharedResourceRoots；
5. 初始化Prompt content cache、policy和diagnostics；
6. Runtime shutdown 时停止新的 source load，并释放 Prompt 子系统资源。

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

Durable Agent/Session definitions与recorded conversation
→ future Turn capture所需exact refs和稳定conversation facts

TurnExecutionContext（process-local）
→ Arc<PromptSet>、Arc<ToolSet>、captured Arc<SkillView>、Arc<WorkspaceSnapshot>和Arc<TurnModelSnapshot>
```

Session execution在candidate admission期间创建PromptSet，并在admission失败或Turn terminal后随Context释放。PromptService不创建Turn，也不修改TurnStatus。完整capture、live conversation assembly和async loop关系见[Turn执行上下文](turn-execution-context.md)。

## PromptService

PromptService对外隐藏Prompt source discovery、共享view publication、内容加载、cache、selection解析、排序、policy、校验和diagnostics：

```rust
pub struct PromptService {
    sources: Vec<Arc<dyn PromptSourceAdapter>>,
    workspace_sources: Vec<Arc<dyn WorkspacePromptSourceAdapter>>,
    content_cache: PromptContentCache,
    policy: PromptPolicy,
    diagnostics: PromptDiagnostics,
}
```

基础 interface：

```rust
impl PromptService {
    pub(crate) async fn initialize(&self) -> Result<Arc<PromptResourceView>, PromptError>;

    pub(crate) async fn build_reload_candidate(
        &self,
    ) -> Result<Arc<PromptResourceView>, PromptError>;

    pub(crate) async fn capture_workspace_sources(
        &self,
        context: WorkspacePromptCaptureContext,
    ) -> Result<Arc<[CapturedWorkspacePromptSource]>, PromptError>;

    pub(crate) async fn for_turn(
        &self,
        context: PromptTurnContext,
    ) -> Result<Arc<PromptSet>, PromptError>;
}
```

`initialize()`构建并返回第一个shared `PromptResourceView`，读取、解析并materialize所有required shared Prompt source content，不创建Agent、Session或Turn。`build_reload_candidate()`只准备并校验包含完整`PromptContent`的candidate，不发布。PromptService不保存current pointer，也没有publish方法；Runtime把candidate放入完整`SharedResourceRoots`后一次publication。任一Prompt/Skill/Tool/Model required candidate失败时old roots全部保持不变。watcher最多标记dirty，不自动publication。

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
pub(crate) trait PromptSourceAdapter: Send + Sync {
    async fn discover(&self) -> Result<Vec<PromptDefinition>, PromptSourceError>;
}

pub(crate) trait WorkspacePromptSourceAdapter: Send + Sync {
    async fn capture(
        &self,
        context: &WorkspacePromptCaptureContext,
    ) -> Result<Vec<CapturedWorkspacePromptSource>, PromptSourceError>;
}
```

首版shared adapter只需要Runtime built-in和用户配置source；Workspace adapter只在Session load、Idle definition update或`/reload workspace` candidate阶段运行。AgentDefinition与SessionDefinition引用shared definition，不拥有独立source adapter。adapter配置可以包含path、URL或built-in preset，但`discover()`/`capture()`成功返回前必须读取并materialize正文；source locator不能穿过该seam成为Turn执行期resolver。

Workspace project instructions不进入全局共享PromptResourceView。其filesystem source在Session load、Idle Workspace definition update或显式`/reload workspace`的candidate阶段经授权读取、解析、规范化并捕获为不可变text Arc；成功publication后由Turn-pinned `WorkspacePromptContext`直接携带。`for_turn()`只选择该context中已经materialize的captured values并构造或复用PromptContent，不在Turn内解析source locator、按path读取current file或执行正文I/O。该context包含canonical cwd、primary root和已授权captured Prompt sources；它不包含write capability，也不能从filesystem-readable additional roots自行扩大Prompt source。active Turn期间Workspace definition不热更新；Session lifecycle candidate operation负责source capture cancellation，并在publication前重新验证current authority/revision。完整定义见[Workspace子系统架构设计](workspace.md)。

`capture_workspace_sources()`只接受Workspace candidate私有投影出的`WorkspacePromptCaptureContext`，只能在其中authorized Prompt roots内discover/read，并返回带model-safe relative location与exact authorization/provenance的immutable captured values。它不发布Snapshot；Session lifecycle只有在Workspace resolve、Prompt capture和Skill capture全部成功后才原子发布new WorkspaceSnapshot。

## PromptDefinition

```rust
pub struct PromptDefinition {
    pub id: PromptId,
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

#[derive(Clone)]
pub struct PromptContent {
    text: Arc<str>,
}

impl PromptContent {
    pub fn text(&self) -> &str;
}
```

`PromptContent`字段与constructor保持private，是candidate build期间已经capture、解析和规范化的materialized immutable text value。clone只复制强`Arc`；同正文可以内部共享同一个`Arc<str>`，但PromptDefinition的identity、role与provenance保持独立。

cache可以按captured source object与parser输入复用正文，也可以在每次reload直接清空；eviction只删除future reuse机会，不使已发布PromptResourceView、PromptDefinition或PromptSet失效。correctness不能依赖PromptId、path、hash、cache hit或额外version。Workspace project instruction保留独立`WorkspaceSourceRef`用于provenance，不用于重新读取正文；SecurityRevoked获胜后，active Turn不得再次使用该PromptSet发起模型调用，terminal后new Turn必须从重新resolved Workspace捕获new context。

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

`PromptId`是selection使用的稳定key；PromptDefinition实际正文属于当前immutable PromptResourceView。shared `/reload`可以在PromptId不变时发布new definition object，active PromptSet继续持有old object，future Turn捕获new object，不为该替换增加version或generation。

## 共享资源 View

```rust
pub(crate) struct PromptResourceView {
    definitions: HashMap<PromptId, Arc<PromptDefinition>>,
}
```

PromptService是共享PromptDefinition的owner。Agent和Session只保存PromptId selection；同一definition可以被任意多个Session使用而不复制正文或加载状态。

selection只适用于普通可选Prompt。Runtime required policy始终由PromptService加入，不出现在selection中。Agent selection只能选择声明为System且来源可信的definition；Session selection只能选择User definition。role不匹配或PromptId不存在时返回typed error，不能静默忽略。

PromptResourceView发布后不可变，并通过PromptDefinition强持有全部PromptContent。shared `/reload`成功后在同一个Runtime publication gate内与Skill/Tool/Model current roots一起替换；已经创建的PromptSet继续持有旧definition/content `Arc`，future Turn捕获新view。cache eviction或source删除不能使old view失效。

## 外部窄 View

PromptService不接收ToolService、ToolSet、SkillService或完整SkillView handle，只接收模型安全的只读view。`ToolPromptView`由Tools模块定义且只能由parent ToolSet构造；`SkillPromptView`由Skills模块定义且只能由parent SkillView构造。二者字段与constructor都private，只暴露只读slice getter，因此调用方不能从任意spec/metadata数组构造替代view。ToolPromptView不能执行Tool，也不暴露executor、approval、policy或Sandbox。MVP没有独立guidelines字段；PromptProfile只从Direct ToolSpec的name/description按ToolName投影User metadata。

SkillPromptView由Turn捕获的SkillView投影。它不包含完整Skill正文，也不能通过PromptService加载Skill。

## PromptTurnContext

Turn 编排层交给 PromptService 的输入：

```rust
pub struct PromptTurnContext {
    agent: AgentRevisionRef,
    session_id: SessionId,
    session_revision: SessionDefinitionRevision,
    resources: Arc<PromptResourceView>,
    agent_prompts: AgentPromptSelection,
    session_prompts: SessionPromptSelection,
    workspace: WorkspacePromptContext,
    model: Arc<TurnModelSnapshot>,
    tools: ToolPromptView,
    skills: SkillPromptView,
}
```

字段与constructor保持crate-private。只有TurnExecutionContext capture能从同一个captured `SharedResourceRoots`、WorkspaceSnapshot、parent ToolSet和parent SkillView构造该context；PromptService不接受普通caller自行拼装的PromptTurnContext。

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

PromptSet 是某个 Turn 的不可变有效 Prompt 快照，由 PromptService private constructor 创建并以 `Arc<PromptSet>` 交给 TurnExecutionContext：

```rust
pub(crate) struct PromptSet {
    resources: Arc<PromptResourceView>,
    profile: PromptProfile,
    definitions: Arc<[EffectivePromptDefinition]>,
    tools: ToolPromptView,
    model: Arc<TurnModelSnapshot>,
}
```

```rust
impl PromptSet {
    pub(crate) fn compose_user_message(
        &self,
        input: UserMessageCompositionInput,
    ) -> Result<CanonicalUserMessage, PromptError>;

    pub(crate) fn assemble(
        &self,
        input: PromptAssemblyInput<'_>,
    ) -> Result<AssembledModelContext, PromptError>;
}
```

PromptSet 在同一个 Turn 中不原地修改。shared Prompt source只能通过显式`/reload`发布新content；Workspace-bound Prompt source只能通过Session load、Idle definition update或显式`/reload workspace`发布。两者都只影响future PromptSet。`for_turn()`和`assemble()`只读取已经materialize的PromptContent，不访问filesystem/network、不按source locator或cache key查找正文。

`compose_user_message()`同样是同步、确定、纯内存operation。它只接收TurnExecutionContext已经异步resolve并完成exact authorization的private input；不得调用SkillService、await source loader、读取current path或处理candidate cancellation。Cancel/SecurityRevoked与await后basis重验由Session Execution拥有。

## PromptProfile

```rust
pub struct PromptProfile {
    pub system: Arc<[PromptSection]>,
    pub user_context: Arc<[PromptSection]>,
}
```

PromptProfile保存已经按固定层级和稳定顺序解析完成的Prompt baseline。`system`只包含Runtime required/base policy与Agent behavior；`user_context`包含Session、Workspace、Tool说明性metadata和Skill metadata，并在AgentRun assembly时编码为位于sanitized live conversation之前的确定性User context。每个PromptSection自带definition provenance/source authorization；它与UserMessage中的PromptContributionStamp不是同一类provenance。SkillPromptView metadata在创建PromptProfile时被稳定渲染，并由parent SkillView私有投影保证来源。

固定顺序：

```text
1. Runtime required system policy
2. Runtime base system Prompt
3. Agent system instructions
4. Session user instructions
5. Workspace user instructions
6. ToolPromptView中由Direct ToolSpec name/description确定性投影的User metadata
7. SkillPromptView user metadata
```

该顺序参考Codex和Claude Code的固定层级：高信任层的位置不能被低层配置或文件扫描顺序改变。不定义`priority`字段。

各层使用与其数据类型一致的稳定顺序：

```text
Runtime/Agent/Session selected PromptDefinition
→ PromptKey → PromptId → provenance source key

Workspace instructions
→ model-safe relative path

ToolPromptView metadata
→ ToolName

SkillPromptView metadata
→ SkillId
```

source adapter返回顺序、filesystem枚举顺序和HashMap迭代顺序都不能影响结果。PromptDefinition层内出现相同`PromptKey`时返回`PromptErrorKind::DuplicateKey`并拒绝创建PromptSet；不静默覆盖或选择“最后发现”的definition。

## PromptIntent 和 CanonicalUserMessage

**Canonical cross-module invariant: INV-202.**

所有prompt-producing输入先归一为“用户body + ordered Skill selections”的PromptIntent：

```rust
pub struct PromptIntent {
    pub body: PromptBodyIntent,
    pub skills: Arc<[SkillIntent]>,
}

pub enum PromptBodyIntent {
    Empty,
    Text(TextIntent),
}

pub struct TextIntent {
    pub text: String,
}

pub struct SkillIntent {
    pub skill_id: SkillId,
}

pub struct MessageRecord {
    content: Arc<[MessageContent]>,
}

pub enum MessageContent {
    Text {
        text: Arc<str>,
    },
}

impl MessageRecord {
    pub fn content(&self) -> &[MessageContent];
}
```

用户body与Skill选择正交；不再定义`PromptIntent::Skill`、`PromptIntent::Composite`、`CompositePromptIntent`或未实现的Template variant。多个Skill按`skills`中的声明顺序表达，重复`SkillId`在正文I/O或live apply前失败。`TextIntent`是non-empty user-authored text value；normalization/size limits由[Wire Schema](wire-schema.md#protocollimits-v10)冻结。`SkillIntent`只保存稳定`SkillId`，不能携带name、path、source ref或authorization。Runtime command input使用同样的逻辑形状；slash name和GUI catalog selection必须先解析为SkillId，queue保存intent而不提前展开正文。

MVP `MessageRecord`只包含ordered Text parts；role由拥有它的UserMessage semantic位置确定，不在record内重复保存。constructor保持private，执行safe-text normalization，并使用`ProtocolLimits.prompt.max_message_part_bytes = 131,072`、`max_user_message_bytes = 524,288`和`max_user_message_parts = 64`。image/audio/document或arbitrary JSON content需要future capability和new variant，不能塞进Text。

TurnExecutionContext使用本Turn captured对象解析intent后，PromptSet把PromptIntent和已经授权的typed contributions原子规范化为唯一用户消息：

```rust
pub(crate) struct UserMessageCompositionInput {
    intent: PromptIntent,
    contributions: Arc<[PromptContribution]>,
}

pub struct CanonicalUserMessage {
    message: MessageRecord,
    contribution_stamps: Arc<[PromptContributionStamp]>,
}

impl CanonicalUserMessage {
    pub fn message(&self) -> &MessageRecord;
    pub fn contribution_stamps(&self) -> &[PromptContributionStamp];
}

pub struct PromptContributionStamp {
    content_part_index: u32,
    origin: PromptContributionOrigin,
}

pub enum PromptContributionOrigin {
    Skill {
        skill_id: SkillId,
    },
    Workspace {
        root_key: WorkspaceRootKey,
        relative_location: WorkspaceRelativePath,
    },
}

impl PromptContributionStamp {
    pub fn content_part_index(&self) -> u32;
    pub fn origin(&self) -> &PromptContributionOrigin;
}
```

CanonicalUserMessage、UserMessageCompositionInput和PromptContributionStamp字段/constructor保持private。live composition constructor只能由PromptSet成功规范化后调用，并要求除optional body外每个contribution part都有exact stamp。Conversation codec使用独立owner-validated replay reconstruction seam：它允许坏stamp被逐项丢弃后留下多个unstamped parts，但仍校验所有surviving stamp的index/order/origin；stamp降级绝不能丢弃合法MessageRecord正文。CanonicalUserMessage是可以进入live conversation并被best-effort record的标准值；它不是裸字符串，也不是与MessageRecord并列的第二份消息状态。live与storage共同使用同一个PromptContributionStamp类型，不定义`StoredPromptContributionStamp`。[Format V1 User Message](../formats/conversation-jsonl-v1.md#user-message)冻结parts/stamps camelCase wire、independent stamp degradation和exact limits。

SkillIntent的完整Skill内容必须先由TurnExecutionContext的async `resolve_user_message()`使用本Turn捕获的SkillView entry调用SkillService加载，并经SkillInjector转换为PromptContribution。TurnExecutionContext确认全部required Workspace contributions成功后才构造composition input。PromptSet不读取Skill文件；它把SkillIntent与Skill contributions一一匹配，验证全部supplied Workspace contributions并规范化进MessageRecord。Skill缺失、stale selection、重复Skill、额外Skill contribution、required Workspace contribution失败或source mismatch时，整条composition在live apply前失败；不能创建部分用户消息。

canonical content顺序固定为：非空body产生的顶层parts在前；Skill contributions按intent顺序；Workspace contributions按`(WorkspaceRootKey, WorkspaceRelativePath)`排序。每个contribution形成一个独立顶层`MessageContent` part，且恰有一个stamp。`content_part_index`是`MessageRecord.content`顶层part数组的零基`u32`索引，不是byte、Unicode scalar、grapheme或rendered text offset；body part不带contribution stamp。`body = Empty`只在至少存在一个合法contribution时有效。

同样的规范化规则可以服务于 Steer control fact，但 storage/domain fact kind 决定它是否开启新 Turn；模型 role 不能反向决定领域 Turn 边界。

## Provider-neutral `ModelMessage`

`ModelMessage`是Prompt拥有的唯一provider-neutral transcript shape。它是**crate-private opaque immutable value**，不是Storage/Wire/Compaction DTO、Runtime external API或public protocol value，也不携带UI、durability、执行或禁止的provider-attempt事实。owned `ModelMessageKind`/`ModelAssistantContentKind` variants、fields和constructors保持Prompt private；**只有Prompt construct/destructure private transcript kinds**。crate-private borrowed read-ref enums是唯一的读取例外：被授权的canonical consumer——ModelGateway的ProviderAdapter、Compaction estimator/reduction，以及Prompt assembly/tests——只能经`ModelMessageRef`/`ModelAssistantContentRef` inspect，不能match private kind、读取field或构造替代transcript。refs从不提供stamp；stamp通过refs仍不可能访问：

```rust
#[derive(Clone)]
pub(crate) struct ModelMessage {
    kind: ModelMessageKind,
}

#[derive(Clone)]
enum ModelMessageKind {
    User { message: CanonicalUserMessage },
    Assistant { content: Arc<[ModelAssistantContent]> },
    Tool { tool_call_id: ToolCallId, content: ToolResultContent },
}

#[derive(Clone)]
pub(crate) struct ModelAssistantContent {
    kind: ModelAssistantContentKind,
}

#[derive(Clone)]
enum ModelAssistantContentKind {
    Reasoning(ReasoningContent),
    Text(Arc<str>),
    ToolCall {
        tool_call_id: ToolCallId,
        name: ToolName,
        arguments: BoundedJsonObject,
    },
}

pub(crate) enum ModelMessageRef<'a> {
    User { content: &'a [MessageContent] },
    Assistant { content: &'a [ModelAssistantContent] },
    Tool {
        tool_call_id: &'a ToolCallId,
        content: &'a ToolResultContent,
    },
}

pub(crate) enum ModelAssistantContentRef<'a> {
    Reasoning(&'a ReasoningContent),
    Text(&'a str),
    ToolCall {
        tool_call_id: &'a ToolCallId,
        name: &'a ToolName,
        arguments: &'a BoundedJsonObject,
    },
}

impl ModelMessage {
    pub(crate) fn canonical_user(message: CanonicalUserMessage) -> Self;
    pub(crate) fn unstamped_user_text(text: Arc<str>) -> Result<Self, ModelMessageError>;
    pub(crate) fn rolling_summary(summary: Arc<str>) -> Result<Self, ModelMessageError>;
    pub(crate) fn assistant(
        content: Arc<[ModelAssistantContent]>,
    ) -> Result<Self, ModelMessageError>;
    pub(crate) fn tool_result(tool_call_id: ToolCallId, content: ToolResultContent) -> Self;

    pub(crate) fn as_ref(&self) -> ModelMessageRef<'_>;
}

impl ModelAssistantContent {
    pub(crate) fn reasoning(content: ReasoningContent) -> Self;
    pub(crate) fn text(text: Arc<str>) -> Result<Self, ModelMessageError>;
    pub(crate) fn tool_call(
        tool_call_id: ToolCallId,
        name: ToolName,
        arguments: BoundedJsonObject,
    ) -> Self;

    pub(crate) fn as_ref(&self) -> ModelAssistantContentRef<'_>;
}
```

`ModelMessage`与`ModelAssistantContent`都是immutable `Clone` values：其variable content由immutable `Arc`-backed value承载，clone不改变semantic identity、content order或provenance，也不重新校验、重排或生成新事实。clone是唯一documented projection：LiveSessionState可以把同一semantic message clone到stable unit和flattened `LiveConversationView`，而不是从borrowed message、raw text或caller suffix重建它。该shared-value规则不放宽private kinds；任何上述authorized consumer仍只能inspect refs。

`rolling_summary()`只可由M4 crate-test-only `CompactionReplacement::for_m4_test()`、M10届时新增的production `ValidatedCompactionSummary → CompactionReplacement` construction，或M5 cold projector从recorded `StoredCompaction`构造replay projection时调用；M5 call永不创建`CompactionReplacement`或调用live reducer。它仍是独立的fallible Prompt constructor，返回redacted `ModelMessageError`：empty为`ModelMessageErrorReason::EmptyText`，UTF-8 byte length超过65,536为`TextTooLong`，unsafe text（包括任意CR或CRLF）为`UnsafeText`。它绝不把CR/CRLF normalize成LF；accepted text逐字保留。rolling summary仍是**恰好一条**unstamped User/Text消息，不加入label、envelope或stamp。

`unstamped_user_text()`只服务PromptSet静态User context，并继续执行它自己的普通safe-text validation；它不复用或暗中继承rolling-summary的65,536-byte/CR规则。两条constructor保持分离。

精确规则：

- User内部保存完整`CanonicalUserMessage`，因此每条User消息的`PromptContributionStamp`仍与该消息局部关联；`ModelMessageRef::User`对任何authorized consumer**只能**给出`content: &[MessageContent]`，没有stamp或任何平行provenance view。
- Assistant只保存上述有序、Prompt-owned `ModelAssistantContent`。`assistant()`拒绝empty content为`ModelMessageErrorReason::EmptyAssistantContent`、duplicate `ToolCallId`为`ModelMessageErrorReason::DuplicateToolCallId`，并保留已validated finalized content order，不重排、合并或制造content block。`ModelAssistantContent::text()`的empty/too-long/unsafe text分别返回`EmptyText`、`TextTooLong`、`UnsafeText`。它不得保存`ItemId`、`EntryId`、`TurnId`、assistant disposition、model、usage、response ID、stream/final index、provider ordering bookkeeping、metadata或Tools-owned `call_index`。
- Tool只保存`ToolCallId + ToolResultContent`；`ItemId`、outcome disposition、entry/turn identity和执行细节不进入它。
- `ModelAssistantContentRef`是Assistant内容的唯一read projection。ToolCall只暴露`tool_call_id`、`name`和`arguments`；它不投影adapter的provider item ID、stream/final index或call-order bookkeeping。
- 完整`ReasoningContent`（`text`、`summary`、`encrypted`、`signature`以及portable `provider_item_id`）作为fixture/storage冻结的reasoning artifact保留。`provider_item_id`是唯一明示允许的portable provider exception：它不是response ID、attempt identity或ordering fact，且只随ReasoningContent作为opaque correlation artifact保留。response IDs、stream/final indices、provider ordering bookkeeping、metadata、usage和所有其他provider-attempt facts禁止进入ModelMessage/ModelAssistantContent。

所有`ModelMessage`、`ModelAssistantContent`、read-ref和Prompt transcript factory的`Debug`必须redact正文、reasoning artifact、arguments、Tool result和stamp provenance。Live reducer与M5 replay只能调用上述Prompt-owned constructors，从canonical live/stored facts建立或投影`ModelMessage`；Compaction、Conversation Storage和Wire可以保存各自的事实，但不得定义shadow transcript type、construct/destructure private kinds或自行转换provider消息。

## PromptContribution

PromptContribution 表示在输入规范化前由其他深模块产生的 typed 模型材料。它不是每次模型调用都可以临时追加的旁路：

```rust
pub struct PromptContribution {
    source: PromptContributionSource,
    content: MessageContent,
}

impl PromptContribution {
    pub fn source(&self) -> &PromptContributionSource;
    pub fn content(&self) -> &MessageContent;
}

pub enum PromptContributionSource {
    Skill(SkillContributionRef),
    Workspace(WorkspaceSourceRef),
}

pub struct WorkspaceSourceRef {
    relative_location: WorkspaceRelativePath,
    authorization: WorkspaceSourceAuthorization,
}

impl WorkspaceSourceRef {
    pub(crate) fn relative_location(&self) -> &WorkspaceRelativePath;
    pub(crate) fn authorization(&self) -> &WorkspaceSourceAuthorization;
}

impl CapturedWorkspacePromptSource {
    pub(crate) fn source_ref(&self) -> WorkspaceSourceRef;
}
```

`WorkspaceSourceRef`字段和constructor保持private，只能从`CapturedWorkspacePromptSource::source_ref()`得到。它携带exact root/source provenance、model-safe relative path和source authorization values；不能使用裸绝对`PathBuf`表达已授权Workspace contribution。PromptSet成功验证后只投影`Workspace { root_key, relative_location }`safe origin。

`SkillContributionRef`携带SkillId和exact source authorization provenance。TurnExecutionContext负责确认entry来自本Turn捕获的SkillView，并且SkillService lazy parse只能使用entry captured bytes；PromptSet成功验证后只投影`Skill { skill_id }`safe origin。exact SkillSourceRef、WorkspaceSourceAuthorization、canonical root、trust、绝对路径和captured bytes都不能进入stamp。

PromptContribution字段和constructor保持private，只能由已授权producer seam创建；它固定为User内容，不能声明System role。producer负责I/O、加载和错误分类；PromptSet只验证、排序并把它固化到`CanonicalUserMessage`或Steer user message。用户显式选择Skill属于该消息规范化，不创建独立Item；模型触发的Skill Tool输出走truthful role=tool message，并在同一assistant全部matching results存在后随complete Tool exchange进入conversation，不形成未归属的PromptContribution lane。

Required contribution 获取失败必须显式返回 unavailable/error，不能通过 vector 缺项静默忽略。

基础生命周期：

```text
PromptIntent + exact authorized PromptContribution
→ PromptSet 输入规范化
→ User MessageRecord + safe part-level PromptContributionStamp
→ LiveConversation typed apply
→ await SessionRecorder.record
→ 后续assembly只从sanitized live conversation重建
```

Turn-static Workspace Prompt、ToolPromptView和SkillView metadata在PromptSet创建时固定，不经过每次调用的PromptContribution。未来若引入动态Context provider，其输出也必须先经过同一规范化与live conversation apply规则，不能恢复current-call assembly旁路。

## 模型上下文组装

每次模型调用的输入只包含sanitized live conversation和typed call policy：

```rust
pub(crate) struct PromptAssemblyInput<'a> {
    kind: PromptAssemblyInputKind<'a>,
}

enum PromptAssemblyInputKind<'a> {
    AgentRun {
        conversation: &'a LiveConversationView,
        output_contract: Option<&'a OutputContract>,
    },
    CompactionSummary {
        source: &'a CompactionSummarySourceView,
        directive: &'a CompactionSummaryDirective,
    },
}

impl<'a> PromptAssemblyInput<'a> {
    pub(crate) fn agent_run(
        conversation: &'a LiveConversationView,
        output_contract: Option<&'a OutputContract>,
    ) -> Self;

    pub(crate) fn compaction_summary(
        source: &'a CompactionSummarySourceView,
        directive: &'a CompactionSummaryDirective,
    ) -> Self;
}
```

`PromptAssemblyInput`、its private kind and both constructors are crate-private. variant确定`ModelCallPurpose`，caller不能把Compaction source伪装成AgentRun input。M4的AgentRun只接受由live state构造的`LiveConversationView`，它已隔离orphan/incomplete Tool exchange；不在M4定义generic live/replay trait。M5若出现第二producer，必须先定义独立explicit input/projection contract，不能retrofit caller-provided transcript trait。`CompactionSummarySourceView`只能由exact `CompactionPlan`从reducer-owned stable-unit prefix派生，包含待摘要prefix的确定性reduced representation，不包含retained suffix。PromptSet assembly不接收裸`Vec<MessageRecord>`、任意ToolPromptView或任意PromptContribution。ConversationRevision和recording规则见[Conversation Recording与Replay](conversation-storage.md)，stable units、cut与marker规则见[Compaction](compaction.md#stable-unit-source)。

`CompactionSummary`固定`OutputContract::NoToolCalls`和empty ToolSpec，只组装Runtime required System policy、typed User summary directive和sanitized reduced prefix source。directive中的effective summary budget必须来自Compaction plan，并与pinned `TurnModelSnapshot` exact limits一起进入assembly proof；PromptSet不能重新clamp或扩大。普通Agent/Session/Workspace/Tool/Skill静态内容不进入摘要请求；下一次`AgentRun` assembly重新注入同一个Turn-pinned PromptSet内容。

planning前，TurnExecutionContext从同一个PromptSet取得两个窄的固定开销basis：

```rust
pub(crate) struct AgentRunCompactionAssemblyBasis {
    fixed_input_tokens: u64,
    rolling_summary_message_overhead_tokens: u64,
    estimator: TokenEstimator,
}

pub(crate) struct CompactionSummaryAssemblyBasis {
    fixed_prompt_tokens: u64,
    system_sections: Arc<[PromptSection]>,
    output_contract: OutputContract,
    estimator: TokenEstimator,
}

impl PromptSet {
    pub(crate) fn agent_run_compaction_assembly_basis(
        &self,
    ) -> AgentRunCompactionAssemblyBasis;

    pub(crate) fn compaction_summary_assembly_basis(
        &self,
    ) -> CompactionSummaryAssemblyBasis;
}
```

`AgentRunCompactionAssemblyBasis.fixed_input_tokens`覆盖exact next ordinary AgentRun中conversation之外的System、Turn-static User context、ToolSpec和structural framing；`rolling_summary_message_overhead_tokens`覆盖future user-role historical summary message除正文之外的开销。该basis与Compaction对stable units的估算相加，必须形成final AgentRun input estimate的conservative upper bound。

`CompactionSummaryAssemblyBasis`只覆盖Runtime required summary System policy、`NoToolCalls` output contract和empty ToolSpec的固定组装开销；不包含summary source、directive正文或任意dynamic contribution。两个basis都使用PromptSet持有的`TurnModelSnapshot::token_estimator()`。Compaction负责把它们、candidate-specific reduced source/directive、pinned model limits和settings reserve合成为最终`CompactionSummaryBudget`。最终assembly必须复算并验证basis exact structural values与实际sections一致，并验证plan、PromptSet和TurnModelSnapshot使用同一个TokenEstimator。

最终输出：

```rust
pub(crate) struct AssembledModelContext {
    system: Arc<[PromptSection]>,
    messages: Arc<[ModelMessage]>,
    tools: Arc<[ToolSpec]>,
    output_contract: Option<OutputContract>,
    diagnostics: Arc<[PromptDiagnostic]>,
    assembly_proof: PromptAssemblyProof,
}

impl AssembledModelContext {
    pub(crate) fn system(&self) -> &[PromptSection];
    pub(crate) fn messages(&self) -> &[ModelMessage];
    pub(crate) fn tools(&self) -> &[ToolSpec];
    pub(crate) fn output_contract(&self) -> Option<&OutputContract>;
    pub(crate) fn diagnostics(&self) -> &[PromptDiagnostic];
    pub(crate) fn assembly_proof(&self) -> &PromptAssemblyProof;
}

pub(crate) struct PromptAssemblyProof {
    purpose: ModelCallPurpose,
    turn_model: TurnModelRef,
    source_revision: ConversationRevision,
    output_contract: Option<OutputContract>,
    compaction_summary_budget: Option<CompactionSummaryBudgetProof>,
}

impl PromptAssemblyProof {
    pub(crate) fn purpose(&self) -> ModelCallPurpose;
    pub(crate) fn turn_model(&self) -> &TurnModelRef;
    pub(crate) fn source_revision(&self) -> ConversationRevision;
    pub(crate) fn output_contract(&self) -> Option<&OutputContract>;
    pub(crate) fn compaction_summary_budget(
        &self,
    ) -> Option<&CompactionSummaryBudgetProof>;
}

pub(crate) struct CompactionSummaryBudgetProof {
    max_output_tokens: NonZeroU32,
    budget: CompactionSummaryBudget,
}

impl CompactionSummaryBudgetProof {
    pub(crate) fn max_output_tokens(&self) -> NonZeroU32;
    pub(crate) fn budget(&self) -> &CompactionSummaryBudget;
}
```

AssembledModelContext是唯一允许进入ModelGateway的crate-private provider-neutral Prompt输出。所有fields保持private；ModelGateway只经上述narrow crate-private getters读取，provider adapter再经`ModelMessage::as_ref()`读取transcript。`system`只保存有序System section；Session/Workspace/Skill等User context已经确定性地位于`messages`前部。它没有unscoped flat `contribution_stamps`字段：stamp只留在各个User `ModelMessage`内部，且不是provider payload、cache-control input、source locator或authorization。`assembly_proof`是crate-private consistency proof，不是第二个caller-controlled purpose；`ModelCallRequest::new(...)`用getter校验purpose、exact `TurnModelRef`、source ConversationRevision、OutputContract binding，以及CompactionSummary request max output与exact budget values。AgentRun的`compaction_summary_budget = None`，CompactionSummary必须为`Some`。provider原生System字段、User message和cache-control encoding由[ModelGateway](model-gateway.md)处理。

`ModelMessage`的构造和provider read projection只由Prompt提供；`PromptSet::assemble()`只负责把已经canonical的transcript与本Turn静态context组装成最终`AssembledModelContext`，不垄断构造时机。

## 最终校验

`PromptSet::assemble()` 集中执行：

- System section和前置User context顺序确定；
- required Runtime policy 未缺失；
- PromptKey 和 contribution source 不发生非法重复；
- PromptSet 内绑定的 ToolPromptView 必须是parent ToolSet私有投影；该 cross-binding 在 TurnExecutionContext capture/final validation 时通过对象所有权完成；
- 不存在 orphan ToolResult；
- 不存在非法截断的 unresolved ToolCall；
- conversation中的UserMessage没有被放到ToolCall/ToolResult exchange中间；Compaction产生的historical summary可以覆盖旧initiating/Steer原文；
- live composition中的exact Skill/Workspace source ref已经通过本Turn captured authority校验；MessageRecord只保留safe part-level stamp，cold replay不重新读取或重新授权旧source；
- required contribution 在输入规范化阶段缺失时失败；
- 不存在尚未进入LiveConversation的current-call model-visible contribution；
- output contract 不被伪装成普通 Prompt text；
- CompactionSummary source只能是plan从完整stable-unit prefix派生的reduced view，不包含retained suffix，也不能切开Tool exchange；
- CompactionSummary directive budget与assembly proof exact structural values match，AgentRun不携带Compaction budget proof；
- 最终大小和token estimate不超过有效模型限制；所有estimate使用PromptSet持有的`TurnModelSnapshot::token_estimator()`，不得自定义bytes/token常量。

PromptSet 不自行执行 compaction。超限时返回结构化 PromptError，由 Session execution 决定后续行为。

## 调用流程

```text
MiniCoreRuntime 启动
→ 创建 Arc<PromptService>
→ PromptService.initialize()返回initial PromptResourceView
→ 四个module initial roots全部成功后，Runtime安装SharedResourceRoots

candidate Turn admission
├─ shared publication gate克隆PromptResourceView / SkillResourceView / ToolResourceView / ModelCatalogView
├─ SkillService.for_turn(captured resources, Arc<SkillViewContext>) → Arc<SkillView> → SkillPromptView
└─ ToolService.for_turn(captured tool resources, context) → Arc<ToolSet> → ToolPromptView

三个view均就绪
→ PromptService.for_turn(PromptTurnContext)
→ Arc<PromptSet>

用户输入
→ await TurnExecutionContext.resolve_user_message(PromptIntent)
→ 从captured SkillView按需load / SkillInjector.build
→ 构造crate-private UserMessageCompositionInput
→ PromptSet.compose_user_message(...)（同步纯内存）
→ CanonicalUserMessage
→ apply live initiating UserMessage + inline record attempt

每次模型调用
→ LiveConversationView
→ PromptSet.assemble(...)
→ AssembledModelContext
→ ModelGateway
```

PromptService 不主动调用 ToolService、SkillService 或 ModelGateway。

## 与同类项目的关系

Codex 每次模型调用构造一个最终 `Prompt`，其中包含 conversation input、ToolSpec、base instructions 和 output schema。Skill/plugin injection 先转成 input item，再进入最终 Prompt。MiniCore 保留这种“每次调用形成唯一最终 Prompt”的思路，但把分散在 Turn 流程中的组装规则收敛到 PromptService/PromptSet。

pi 使用纯 `buildSystemPrompt(options)` 组装 custom/base prompt、tools、context files、skills、date 和 cwd。MiniCore 保留其纯确定性组装思路，但使用不可变 PromptSet、typed contribution 和 private Arc ownership，避免 active tool/resource 变化导致局部重建分裂。

Claude Code使用managed、user、project、local和path-scoped instructions，并对Skill内容使用按需加载。MiniCore同样共享资源、按Session构建上下文；只保留System与User两种Prompt role。

## 错误和 Diagnostics

`PromptValueError`是既有的**public** value error，不是PromptService/assembly的宽泛`PromptError`替代；M4不得为transcript construction向它添加variant，也不得改变其既有contract。transcript专用错误是crate-private、private-field/redacted的`ModelMessageError`：

```rust
pub(crate) struct ModelMessageError {
    reason: ModelMessageErrorReason,
}

pub(crate) enum ModelMessageErrorReason {
    EmptyText,
    UnsafeText,
    TextTooLong,
    EmptyAssistantContent,
    DuplicateToolCallId,
}
```

这是M4 transcript construction的完整且最小reason taxonomy。`Debug`、`Display`和source chain均不得回显transcript、ToolCallId、arguments、Tool result或summary text。`ModelMessage::{unstamped_user_text, rolling_summary, assistant}`和`ModelAssistantContent::text`一律返回`Result<_, ModelMessageError>`：replacement/rolling-summary factory只有text input，因此只能达到`EmptyText | UnsafeText | TextTooLong`；assistant constructor独立达到`EmptyAssistantContent | DuplicateToolCallId`。不定义`PromptTranscriptError`、Compaction-owned Prompt error shadow type或任何新的public PromptValueError variant。

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
| PromptResourceView candidate、Prompt definitions/materialized content/source/cache | PromptService |
| complete SharedResourceRoots publication | MiniCoreRuntime |
| AgentPromptSelection | exact AgentDefinition revision |
| SessionPromptSelection / Workspace definition | exact SessionDefinition revision |
| Workspace project Prompt discovery/capture与正文cache | PromptService，经WorkspacePromptCaptureContext授权 |
| Arc<PromptSet> | Turn 执行上下文 |
| ToolPromptView | ToolSet 投影 |
| SkillPromptView | SkillView 投影 |
| LoadedSkill → PromptContribution | SkillInjector；由TurnExecutionContext async resolve保证view来源和读取授权 |
| PromptContribution → live MessageRecord | PromptSet 输入规范化 |
| conversation | Session conversation owner |
| CanonicalUserMessage composition / final context assembly | PromptSet |
| `ModelMessage` construction与provider-facing projection | Prompt（PromptSet只执行final assembly） |
| provider payload encoding | ModelGateway |

## 基础不变量

- 一个 MiniCoreRuntime 初始化一个 PromptService；
- PromptService拥有共享PromptResourceView；AgentDefinition和SessionDefinition只保存PromptId selection；
- PromptContent在candidate build期间完全materialize，并由PromptResourceView/PromptSet强Arc持有；
- path、URL、source ID、hash和cache key都不能成为`for_turn()`/`assemble()`的正文resolver；
- 同一个PromptDefinition可以被多个Session选择，但每个Turn独立构造PromptSet；
- Prompt capture使用exact AgentRevisionRef、SessionDefinitionRevision和当时的current PromptResourceView，不读取Agent current；
- Turn 领域对象不持有完整 PromptSet；
- TurnExecutionContext 在本 Turn 内复用同一个不可变 PromptSet；
- PromptSet在创建时固定PromptResourceView、ToolPromptView和渲染后的SkillPromptView metadata；
- PromptService 不主动调用 ToolService、SkillService 或 ModelGateway；
- PromptSet 不执行 Tool、不加载 Skill、不读写 conversation storage；
- PromptSet compose/assemble均同步纯内存；async contribution resolve与cancel/revalidation不进入Prompt module；
- Runtime required policy不进入selection，不可被Agent或Session关闭；
- Prompt role只保留System和User；Runtime/Agent可信行为进入System，Session/Workspace/Skill进入User；
- Workspace file readable 不等于可作为 Prompt source；
- PromptService只能在candidate阶段从WorkspacePromptCaptureContext授权roots读取project Prompt；`for_turn()`只解析WorkspacePromptContext中的captured sources；
- Workspace project instruction在composition前必须保留typed WorkspaceSourceRef/source authorization provenance和captured content，cache不能只按path复用；成功后conversation只保存root-relative safe origin；
- Prompt baseline使用固定信任层顺序；层内使用stable typed keys全序，不存在caller-controlled priority；
- 同一固定层内重复PromptKey fail closed；
- `ModelMessage`构造/provider projection只有Prompt-owned入口；PromptSet只有最终assembly职责；
- M4 AgentRun assembly只接受LiveSessionState owner-created sanitized `LiveConversationView`，CompactionSummary只接受CompactionPlan提供的reduced stable-prefix view；M5 must add any replay producer explicitly before a generic shared view exists。两者都不接受任意Tool view、orphan/incomplete Tool exchange或current-call contribution；
- AssembledModelContext 是进入 ModelGateway 的唯一 Prompt 输出；
- 相同输入产生相同排序和输出；
- Prompt reload先构建candidate PromptResourceView，四个shared candidates全部成功后在Runtime publication gate内替换current root；不原地修改active PromptSet。
- Prompt content cache只优化future reuse；清空或eviction不影响任何已发布view或active Turn。
- PromptIntent的body与skills正交；重复Skill、missing/stale Skill或required contribution失败时不产生部分UserMessage；
- exact source authorization只存在于composition前校验；每个contribution形成独立顶层part，conversation只保留safe part-level stamp；
- replay不得通过stamp重新读取正文或重新授权source，stamp损坏不能导致合法conversation正文丢失。

## 测试要求

- Text body + one Skill、empty body + one Skill和ordered multiple Skills产生稳定part顺序；
- MessageRecord只接受1..64个bounded safe Text parts，body/contribution aggregate超limit时不apply；
- duplicate SkillId、captured Skill缺失/删除、source mismatch或required Workspace contribution失败时不apply部分UserMessage；
- reload发生在active Turn期间时，Steer继续从captured SkillView加载旧entry；future Turn使用new view；
- resolve等待Skill load时Cancel/SecurityRevoked由Session execution终止caller，迟到cache结果不能进入PromptSet/live conversation；
- PromptContributionStamp format-v1 golden：Skill/Workspace origin、unknown/malformed/out-of-range/duplicate first-valid behavior；
- Workspace contributions按`WorkspaceRootKey + WorkspaceRelativePath`稳定排序；
- Unicode正文、emoji和Image等结构化part不影响`content_part_index`关联；
- stamp不包含绝对路径、canonical root、trust、authorization、hash、cache key或正文引用；
- CanonicalUserMessage与StoredUserMessage之间没有第二份stamp类型或重复字段。
- User/Assistant/Tool和rolling-summary `ModelMessage`只可通过Prompt-owned constructors构造；private kind只能由Prompt destructure。`ModelMessage`与`ModelAssistantContent`是immutable Arc-backed `Clone` values；clone保持semantic identity/order/provenance，可将同一message投影到stable unit和flattened `LiveConversationView`，但不允许reconstruction。所有transcript structs、read-ref enums和`as_ref()`均是`pub(crate)`，不是Runtime/external API；ProviderAdapter、Compaction estimator/reduction和Prompt assembly/tests仅经refs读取。`ModelMessageRef`/`ModelAssistantContentRef`精确投影role/content/ToolCallId/ToolResultContent，User没有stamp且stamp不能通过refs取得；ReasoningContent只允许portable provider_item_id exception，response ID、stream/final index/order、metadata和usage均不泄漏，Debug保持redacted。
- `PromptValueError`维持原有public variants不变。transcript constructors返回private redacted `ModelMessageError`；replacement/rolling-summary tests只覆盖reachable `EmptyText | UnsafeText | TextTooLong`（含CR/CRLF、绝不normalize），并证明accepted summary verbatim且无label/envelope/stamp；assistant constructor tests独立覆盖`EmptyAssistantContent | DuplicateToolCallId`。
- 每条User `ModelMessage`保留自己的stamp；静态PromptSet User context与rolling summary均unstamped，provider payload/cache-control不能读取stamp。
- Live reducer与M5 replay使用Prompt constructors投影canonical facts；Compaction/Storage/Wire不定义shadow transcript或自行做`ModelMessage`转换。
- `PromptAssemblyInput`、`PromptSet::assemble`、`AssembledModelContext`和assembly proofs均为crate-private、private-field interface；ModelGateway只经narrow getters和message read refs消费它们。

## 已关闭问题

1. **Prompt Q1已由ADR 0128关闭**：`PromptContent`是candidate build期间完全materialize的immutable text value，内部使用强`Arc<str>`共享；不定义可重新解析或durable的`PromptContentRef`。
2. **Prompt Q4已由ADR 0129关闭**：PromptIntent使用`body + skills[]`；SkillIntent只保存SkillId；每个contribution对应一个顶层content part；live/JSONL使用同一种safe part-level PromptContributionStamp，不保存字符offset或exact authorization。

## 后续问题

3. PromptResourceView candidate build和content cache eviction实现。
4. Prompt template整体后置：MVP `PromptBodyIntent`只有Empty/Text。future feature必须同时定义stable PromptTemplateId、argument grammar、materialized render、limits、reload/capture和protocol capability，不能只恢复一个未定义enum variant。
5. 未来若ToolSpec description无法表达真实per-tool使用约束，是否新增typed guidelines；MVP不支持独立guidelines字段。
6. Historical PromptSet审计格式；MVP不用于Turn cold resume。
7. Prompt content cache 的 key、eviction 和失效策略。
8. Prompt hook 和动态 Context provider 是否能在不建立未提交模型可见旁路的前提下接入。
9. Provider cache效果和canonical instruction boundary验证。
10. PromptError 与 Turn terminal/compaction 的映射。
