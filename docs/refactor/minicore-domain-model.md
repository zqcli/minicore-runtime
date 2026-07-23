# MiniCore 领域模型

状态：独立领域模型草案
日期：2026-07-16

## 目的

本文从零定义 MiniCore 的目标领域模型，不以任何现有实现、模块划分、协议或存储结构为前提，也不描述从其他架构迁移到本文模型的过程。

本阶段定义以下概念的基础关系、类型、状态和 scope 配置规则：

```text
MiniCoreRuntime
Agent
AgentDefinition
AgentRevisionRef
Session
SessionDefinition
SessionDefinitionRevision
Turn
Item
Interaction
Prompt
PromptService
PromptSet
Tool
ToolService
ToolSet
Skill
SkillService
SkillCatalog
LoadedSkill
SkillInjection
Definition
DefinitionOverrides
LoadState
```

本阶段暂不设计：

- SessionExecutor的具体scheduler/runtime实现；其ownership以[Session Execution架构设计](session-execution.md)为权威；
- 已定义 SessionStorage by-entry JSONL tree 的 adapter 实现，以及 catalog/loaded-runtime registry；
- Model 的内部结构；
- Prompt source、cache、fingerprint 和内容组装的实现细节；
- Tool 的注册、披露、执行、权限和 Sandbox 实现细节；
- provider adapter、具体 retry/backoff、compaction 实现或公开协议 method；
- Item type 的最终封闭集合；
- ID 的具体编码、全局范围和 physical retention/vacuum。

## 决策摘要

MiniCore 的基础领域关系为：

```text
MiniCoreRuntime
└─ Agent*
   └─ Session*
      └─ Turn*
         └─ Item*
            └─ Interaction*
```

基数关系：

```text
MiniCoreRuntime 1 ── N Agent
Agent           1 ── N Session
Session         1 ── N Turn
Turn            1 ── N Item
Item            1 ── N Interaction
```

已经确定：

- `MiniCoreRuntime` 是 MiniCore 的顶层门面；
- 一个 Agent 可以被多个 Session 引用；
- 一个 Session 只能对应一个 Agent；
- Workspace 属于 Session，不属于 Agent 或 Turn；
- Workspace 是 Session-owned definition，不是独立 entity 或 Runtime-global registry entry；
- 当前不定义 `WorkspaceId`；Session owner、WorkspaceRevision 和 WorkspaceFingerprint 分别表达 ownership、definition version 和有效快照 identity；
- loaded Session execution state 保存当前 `Arc<WorkspaceSnapshot>`，Turn execution context pin 同一个不可变快照；
- filesystem access、Prompt source authorization 和 Skill source authorization 是三个独立 grant；
- AgentDefinition 和 SessionDefinition 分别持有自己 scope 下的 Prompt 配置；
- Prompt、Tool、Skill 是独立概念，不合并成通用 `Resource`；
- Prompt 的 scope 类型使用复数名称，明确表示定义集合；
- Definition 保存稳定定义和版本；
- `MiniCoreRuntime` 启动时初始化一个 `Arc<PromptService>`；
- Turn admission 先预留 candidate Turn identity，再创建执行期不可变 `PromptSet`；
- Turn 领域对象不持有 PromptSet 或 Prompt fingerprint；exact Prompt references 属于 start execution metadata；
- PromptSet 负责 CanonicalUserMessage 和最终模型上下文组装；
- `MiniCoreRuntime` 启动时初始化一个 `Arc<ToolService>`；
- Agent、Session 和 Turn 领域对象不持有 Tool 属性；
- candidate admission 在第一次模型调用前调用 `ToolService::for_turn(...)`；
- `ToolService::for_turn(...)` 返回执行期不可变 `ToolSet`，同一 Turn 内重复使用；
- `MiniCoreRuntime` 启动时初始化一个 `Arc<SkillService>`；
- Runtime Service 只注入 loaded Session execution / TurnExecutionContext，不进入 durable Agent 或 Session；
- Skill 暂不区分 Runtime、Agent、Session 或 Turn 等配置层级；
- RuntimePrompts、AgentDefinition、SessionDefinition 保存各自 scope 的 Prompt 配置或稀疏覆盖；
- TurnExecutionContext pin WorkspaceSnapshot、PromptSet、ToolSet、SkillCatalog 和 TurnModelSnapshot；Turn 领域对象不持有这些执行期对象；
- Turn execution 包含并驱动底层 AgentLoop，但 AgentLoop 不拥有 storage、Tool execution、Prompt assembly 或 terminal append；
- 每次逻辑模型调用由committed conversation checkpoint、purpose、output contract、effective max_output_tokens与`AssembledModelContext`确定，不增加ModelStep领域类型、ID或registry；
- Turn 执行期间只通过 pinned SkillCatalogEntryRef 按需加载 Skill，并交给 Injection 层形成可提交的输入材料；
- 实际加载状态不进入 Definition，由对应子系统单独维护；
- 同一个 PromptDefinition 可以在不同 Session 中具有不同的启用和可见状态；
- SessionDefinition pin exact `AgentRevisionRef`；Agent current update 不自动改变既有 Session；
- Turn 不持有 `AgentId`、`AgentRevisionRef`、SessionDefinition 或 Workspace；
- Turn 从 initiating UserMessage entry append 开始，到 final AssistantMessage、TurnInterrupted 或 TurnFailed entry append 结束；
- Steer 是 current Turn control input；FollowUp 在当前 Turn terminal 后开启新 Turn；
- Item 的最小集合是 UserMessage、AgentMessage、Reasoning 和 ToolInvocation；
- ToolCall 与 ToolResult 属于同一个 ToolInvocation Item；
- Interaction 是 Item-owned durable request/resolution，遵守 request-before-notify 和 resolution-before-resume。

## 领域关系

### MiniCoreRuntime

`MiniCoreRuntime` 表示一个完整的 MiniCore runtime 实例，是外部宿主接触 MiniCore 的顶层门面。

它持有 Runtime 生命周期内唯一的 PromptService、ToolService 和 SkillService：

```rust
pub struct MiniCoreRuntime {
    pub prompt_service: Arc<PromptService>,
    pub tools: Arc<ToolService>,
    pub skills: Arc<SkillService>,
}
```

PromptService 的完整设计见 [Prompt 子系统架构设计](prompt-subsystem.md)。

ToolService 的完整设计见 [Tool 子系统架构设计](tool-subsystem.md)。

SkillService 的完整设计见 [Skill 子系统架构设计](skill-subsystem.md)。

Workspace 的完整设计见 [Workspace 子系统架构设计](workspace-subsystem.md)。

本阶段不定义 `MiniCoreRuntime` 的 command、query、event、snapshot interface，也不定义它如何创建、保存或查找 Agent。

### Agent

Agent 是可被多个 Session 引用的 durable entity。Agent head 保存 identity、当前 definition pointer、status 和用户可见 metadata：

```rust
pub struct Agent {
    pub id: AgentId,
    pub current_revision: AgentRevision,
    pub status: AgentStatus,
    pub name: String,
    pub description: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}
```

Agent execution definition 使用 immutable revision value：

```rust
pub struct AgentDefinition {
    pub agent_id: AgentId,
    pub revision: AgentRevision,
    pub prompts: AgentPrompts,
    pub created_at: Timestamp,
}

pub struct AgentRevisionRef {
    pub agent_id: AgentId,
    pub revision: AgentRevision,
}
```

`AgentDefinition` 不是独立 entity；其 identity 是 `(AgentId, AgentRevision)`，发布后不可原地修改。

`AgentRevision` 只在 execution definition canonical content 改变时产生。name、description 或 AgentStatus 改变不产生新 revision；rollback 使用旧内容创建新的更高 revision。

Agent 不持有 Workspace、Session conversation、current Turn、Item、Interaction、provider client、Runtime Service、manager、registry 或 storage handle。

完整 revision、status、update 和 retention 规则见 [Agent 与 Session 生命周期架构设计](agent-session-lifecycle.md)。

### Session

Session 是长期存在的对话对象。Session head 保存 identity、当前 definition pointer、durable lifecycle 和用户可见 metadata：

```rust
pub struct Session {
    pub id: SessionId,
    pub current_revision: SessionDefinitionRevision,
    pub lifecycle: SessionLifecycle,
    pub name: Option<String>,
    pub description: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}
```

Future Turn 的完整 durable 配置属于 immutable SessionDefinition：

```rust
pub struct SessionDefinition {
    pub session_id: SessionId,
    pub revision: SessionDefinitionRevision,
    pub agent: AgentRevisionRef,
    pub workspace: Workspace,
    pub model: SessionModelConfig,
    pub prompts: SessionPrompts,
    pub created_at: Timestamp,
}
```

`SessionModelConfig`只保存ModelSelection、ReasoningPreference和可选max output token preference；provider definition、capabilities、endpoint和credential不进入SessionDefinition。Turn admission通过ModelGateway解析exact TurnModelSnapshot。

一个 Session 的全部 revisions 只能引用同一个 AgentId。升级 Agent 只能选择该 Agent 的另一个 exact revision；切换到另一个 Agent 必须创建新 Session。

Session 创建时 pin Agent 当时的 current revision。Agent 后续发布新 revision 不自动改变 active 或 future Turn；Session 必须通过新 SessionDefinitionRevision 显式升级。

`SessionDefinitionRevision`原子绑定AgentRevisionRef、Workspace、SessionModelConfig和SessionPrompts。Workspace definition 更新同时产生新的 WorkspaceRevision 和外层 SessionDefinitionRevision。

Session 不持有 Runtime Service、WorkspaceSnapshot、SkillCatalog、ToolSet、PromptSet、conversation hot projection 或 active Turn。PromptService、ToolService、SkillService 和 ModelGateway 由 Runtime 注入 loaded Session execution。

`Workspace` 仍是 Session-owned definition，但其字段路径为 `SessionDefinition.workspace`。Workspace 不具有独立 WorkspaceId、registry 或 open/close lifecycle。loaded Session execution state 通过 WorkspaceResolver 生成当前 WorkspaceSnapshot；TurnExecutionContext pin 该快照。

Model 的 identity、字段、状态和生命周期将在后续设计中单独确定。

完整 create/update/upgrade/load/unload/archive/delete/fork 和 recovery 规则见 [Agent 与 Session 生命周期架构设计](agent-session-lifecycle.md)。

### Turn

Turn 表示从 committed initiating UserMessage 到一个 terminal entry 的用户意图执行过程。

```rust
pub struct Turn {
    pub id: TurnId,
    pub session_id: SessionId,
    pub started_at: Timestamp,
    pub status: TurnStatus,
}
```

Turn 逻辑上拥有有序 Items，但 durable Turn head 不内联 `Vec<Item>`；Item 顺序由 SessionStorage projection 提供。

Turn 不直接持有 Agent identity 或 Workspace。active/recovered Turn 通过 initiating UserMessage 引用的 TurnContext entry 解析 exact execution metadata：

```text
TurnId
→ initiating UserMessage.context_entry_id
→ committed TurnContext entry
→ exact SessionDefinitionRevision
→ exact AgentRevisionRef
→ exact Workspace/Prompt/Tool/Skill/Model references
```

`Session.current_revision` 只供 future admission 捕获，不能用于解释 active 或 historical Turn。

Turn 不包含以下字段：

```text
agent_id
agent_revision_ref
agent_snapshot
session_definition
workspace
```

Turn 使用的 exact Model、Prompt、Workspace、Tool、Skill 和 Agent/Session definition references 属于 TurnContext entry，不重复保存在 Turn head。

Turn 领域对象不持有 PromptSet、Prompt fingerprint 或完整 Prompt definitions。Session execution 在 admission 期间捕获 exact SessionDefinitionRevision 和 AgentRevisionRef，再创建执行期不可变 PromptSet。PromptSet 规范化 initiating UserMessage；只有该 UserMessage 成功 append 后领域 Turn 才正式开始。Prompt fingerprint 属于 TurnContext entry。完整规则见 [Prompt 子系统架构设计](prompt-subsystem.md)。

Turn 领域对象不持有 Tool、ToolSet、ToolSpec 或 executor。candidate admission 在第一次模型调用前通过 Runtime 的 `ToolService::for_turn(...)` 创建执行期 `ToolSet`。同一 Turn 内的全部 LLM → Tool → LLM 循环复用该 ToolSet，Turn terminal 后释放。`for_turn` 不创建 Turn，也不修改 TurnStatus。完整规则见 [Tool 子系统架构设计](tool-subsystem.md)。

Turn不持有Skill、SkillCatalog、LoadedSkill或Skill snapshot。Turn execution只使用本Turn pinned Catalog的`SkillCatalogEntryRef`按需加载正文；用户侧SkillInjection必须进入CanonicalUserMessage或Steer，模型触发的Skill Tool输出必须进入role=tool message并由`tool_round_completed`promote，不能成为未append/apply的模型可见旁路。

### Turn Execution

Turn execution 是领域 Turn 外围的执行过程，不增加新的领域层级：

```text
admission reservation
→ TurnExecutionContext capture
→ TurnContext + initiating UserMessage append
→ AgentLoop drive
→ model / Tool / append/apply loop
→ terminal entry append
```

```rust
pub(crate) struct TurnExecutionContext {
    session_id: SessionId,
    session_revision: SessionDefinitionRevision,
    agent: AgentRevisionRef,
    model: TurnModelSnapshot,
    workspace: Arc<WorkspaceSnapshot>,
    skill_service: Arc<SkillService>,
    skill_context: SkillCatalogContext,
    skill_catalog: Arc<SkillCatalog>,
    tool_set: ToolSet,
    prompt_set: PromptSet,
    fingerprint: ExecutionContextFingerprint,
    diagnostics: Arc<[TurnContextDiagnostic]>,
}
```

`TurnExecutionContext` 是不可变 execution binding，不是 Service、领域 entity 或通用 Resource owner。逻辑模型调用只由committed conversation checkpoint、调用purpose、output contract、effective max_output_tokens和`AssembledModelContext`共同确定，不需要新增 `ModelStep` struct、ID、CRUD、registry 或持久生命周期。

Turn execution包含并驱动AgentLoop。`NeedModel / NeedTools / Finished`是private AgentLoop action；Prompt assembly、Tool execution、append/apply和conversation projection、Steer、FollowUp、cancellation及terminal status由SessionExecutor负责。

完整边界、interface、pinning、逻辑模型调用、AgentLoop、fingerprint 和 recovery 规则见 [Turn 执行模块与执行上下文架构设计](turn-execution-context.md)。

### Item

Item 是 Turn 内稳定、可观察的语义值或长生命周期操作：

```rust
pub struct Item {
    pub id: ItemId,
    pub turn_id: TurnId,
    pub content: ItemContent,
}

pub enum ItemContent {
    UserMessage(UserMessageItem),
    AgentMessage(AgentMessageItem),
    Reasoning(ReasoningItem),
    ToolInvocation(ToolInvocationItem),
}
```

`ItemType` 从 ItemContent discriminant 派生，不独立存储：

```rust
pub enum ItemType {
    UserMessage,
    AgentMessage,
    Reasoning,
    ToolInvocation,
}
```

ToolCall 和 ToolResult 属于同一个 ToolInvocation Item：

```rust
pub struct ToolInvocationItem {
    pub call: ToolCall,
    pub state: ToolInvocationState,
}

pub enum ToolInvocationState {
    Started,
    Completed { result: ToolResult },
    Abandoned { reason: ToolAbandonReason },
}
```

UserMessage、AgentMessage 和 Reasoning durable Item 创建时即完成。ToolInvocation 可以从 Started 进入 Completed 或 Abandoned。streaming delta、Tool progress、provider retry 和 execution phase 不是 Item。

Plan、FileChange、CommandExecution 和 Compaction 当前不增加独立 Item variant：Command/FileChange 先作为 ToolInvocation details，Compaction 是 conversation/storage fact；只有出现真实独立 lifecycle 后才扩展。

完整 Item、identity、ToolRound 和 recovery 规则见 [Turn、Item 与 Interaction 架构设计](turn-item-interaction.md)。

### Interaction

Interaction 表示某个 Item 执行期间，由 Runtime 发起并等待外部回答的 durable request/response：

```rust
pub struct Interaction {
    pub request_id: RequestId,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub item_id: ItemId,
    pub request: InteractionRequest,
    pub state: InteractionState,
}

pub enum InteractionState {
    Pending {
        requested_at: Timestamp,
        expires_at: Option<Timestamp>,
    },
    Resolved {
        resolution: InteractionResolution,
        resolution_key: IdempotencyKey,
        resolved_at: Timestamp,
    },
}

pub enum InteractionRequest {
    ToolApproval(ToolApprovalRequest),
    UserQuestion(UserQuestionRequest),
}
```

Interaction 的归属关系固定为：

```text
Interaction → Item → Turn → Session → Agent
```

request必须先append再通知host；resolution必须携带durable resolution key，并先append再唤醒waiter或执行Tool。expires_at在同一append-time validation中校验；transport disconnect不自动关闭Pending Interaction，reconnect使用相同RequestId重发。host restart baseline使用稳定operation key逐entry关闭pending request并中断Turn。

结构化 Interaction response 不是 UserMessage，不开启新 Turn。完整 family、timeout、race 和 recovery 规则见 [Turn、Item 与 Interaction 架构设计](turn-item-interaction.md)。

## Definition

Prompt 由 versioned Definition 表达具体定义。Tool 的 ToolSpec、注册 identity 和执行快照由 [Tool 子系统架构设计](tool-subsystem.md) 单独定义。Skill 的 metadata、完整内容和精确定义身份由 [Skill 子系统架构设计](skill-subsystem.md) 单独定义。

Definition 的基础规则：

```text
DefinitionId 跨版本保持稳定
DefinitionVersion 标识该定义的具体版本
DefinitionId + DefinitionVersion 构成精确定义身份
定义内容变化产生新 DefinitionVersion
加载状态变化不产生新 DefinitionVersion
scope 启用或可见性变化不产生新 DefinitionVersion
```

`DefinitionVersion` 是独立值类型，本阶段不要求使用 SemVer：

```rust
pub struct DefinitionVersion;
```

## Prompt 子系统引用

Prompt 使用独立的 PromptService 架构：RuntimePrompts、AgentDefinition 和 SessionDefinition 保存 scope Prompt 配置；Turn 执行上下文保存解析后的不可变 PromptSet。

基础关系：

```text
MiniCoreRuntime
└─ Arc<PromptService>

AgentDefinition
└─ AgentPrompts

SessionDefinition
└─ SessionPrompts

candidate Turn admission
└─ PromptService::for_turn(PromptTurnContext)
   └─ PromptSet
      ├─ compose_user_message(...) → CanonicalUserMessage
      └─ assemble(committed conversation) → AssembledModelContext
```

Prompt scope 和模型 role 是正交概念：Runtime、Agent、Session 表示配置归属；System、Developer、User 表示模型可见角色。Runtime required policy 不能被低层 scope 覆盖。

PromptService 可以加载 Prompt-specific source、解析 scope overrides、稳定排序并创建 PromptSet，但不拥有 Workspace 生命周期、conversation、Tool executor、Skill loader 或 provider。PromptService 只消费 `ToolPromptView` 和 `SkillCatalogView` 这类窄模型安全 view。

Turn 领域对象不持有 PromptSet；完整 PromptSet 属于 Turn 执行上下文。Prompt fingerprint 进入 Turn start execution metadata。

PromptDefinition、PromptSourceAdapter、PromptTurnContext、PromptSet、PromptIntent、CanonicalUserMessage、PromptContribution、AssembledModelContext、fingerprint 和校验规则以 [Prompt 子系统架构设计](prompt-subsystem.md) 为权威。

## ModelGateway引用

```text
MiniCoreRuntime
└─ Arc<ModelGateway>

SessionDefinition
└─ ModelSelection

candidate Turn admission
└─ ModelGateway.resolve_for_turn(...)
   └─ TurnModelSnapshot

logical model call
└─ ModelGateway.generate_model_turn(ModelCallRequest)
   └─ ModelCallResult | ModelCallError
```

ModelGateway是runtime深模块，不是领域entity。ModelSelection属于Session definition；TurnModelSnapshot属于TurnExecutionContext；provider attempt、stream、connection、auth、cache和continuation都不进入领域模型或SessionStorage。active Turn内不建立ModelStep、ModelAttempt或transparent cross-model fallback。完整规则以[ModelGateway架构设计](model-gateway.md)为权威。

## Tool 子系统引用

Tool 使用独立的 ToolService 架构，不建立 `RuntimeTools / AgentTools / SessionTools / TurnTools` 领域分层。

基础关系：

```text
MiniCoreRuntime
└─ Arc<ToolService>

candidate Turn admission
└─ ToolService::for_turn(ToolTurnContext)
   └─ ToolSet
      ├─ prompt_view() → ToolPromptView
      └─ execute(ToolExecutionRequest[]) → ToolExecutionOutcome[]
```

`ToolService::for_turn(...)` 从 Turn admission 的执行边界开始：Session execution 已预留 candidate Turn identity，但领域 Turn 尚未对外发布。该方法不创建领域 Turn，不改变 TurnStatus，也不属于 Turn 对象。返回的 ToolSet 只存在于执行期；若 initiating UserMessage append 失败，直接随 candidate Context 释放。

领域投影固定为：

```text
ToolCall + ToolResult
→ 同一个 ToolInvocation Item

Tool approval request / decision
→ Interaction，归属于对应 ToolInvocation Item
```

assistant tool_call content使ToolInvocation成为durable Started；matching role=tool message append使它成为Completed operational state。Completed本身不授予模型可见性，只有后续`tool_round_completed`引用完整assistant/tool sequence后才进入模型conversation；executor/side-effect outcome unknown时进入Abandoned，不构造synthetic ToolResult。

Tool 注册、ToolSpec、Direct/Deferred/Hidden 披露、参数校验、Hook、policy、approval、Sandbox、并发和稳定结果顺序以 [Tool 子系统架构设计](tool-subsystem.md) 为权威。

## Skill 子系统引用

Skill 使用独立的 SkillService 架构，不建立 `RuntimeSkills / AgentSkills / SessionSkills / TurnSkills` 领域分层。

基础关系：

```text
MiniCoreRuntime
└─ Arc<SkillService>

loaded Session execution
└─ Runtime 注入 Arc<SkillService>

TurnExecutionContext
└─ pinned SkillCatalogContext + SkillCatalogEntryRef
   └─ SkillService::load
      └─ SkillInjector
         └─ committed user/control/tool material
```

Skill Catalog 只包含名称、描述、路径、作用域和内容 identity 等轻量 metadata。完整 Skill 内容由 SkillService 在 Turn 执行期间确定需要后按需加载、解析并缓存。

Turn对象不持有Skill，也不保存Catalog、LoadedSkill或Skill snapshot。SkillService不决定哪个Turn使用哪个Skill；该决定属于Turn execution。Injection层只负责把已加载内容转换为typed contribution；用户侧contribution由PromptSet规范化进UserMessage/Steer entry，模型触发的Skill Tool输出走tool message + `tool_round_completed`路径。

Skill 子系统的对象、interface、渐进披露、cache、失效和 diagnostics 规则以 [Skill 子系统架构设计](skill-subsystem.md) 为权威。

## Scope 配置

本节的 scope 配置只适用于 Prompt。Tool 的本 Turn 选择和披露由 `ToolService::for_turn(...)` 根据执行上下文完成，见 [Tool 子系统架构设计](tool-subsystem.md)。Skill 当前不建立 Runtime、Agent、Session 或 Turn 配置层级，其过滤和加载规则见 [Skill 子系统架构设计](skill-subsystem.md)。

PromptDefinition 本身不保存某个 Agent、Session 或 Turn 的启用、可见或加载状态。

RuntimePrompts、AgentDefinition 和 SessionDefinition 分别保存各自 scope 的 defaults/overrides：

```rust
pub struct DefinitionOverrides {
    pub enabled: Option<bool>,
    pub user_visible: Option<bool>,
    pub model_visible: Option<bool>,
    pub load_policy: Option<LoadPolicy>,
}
```

字段含义：

| 字段 | 含义 |
| --- | --- |
| `enabled` | 当前 scope 是否允许使用该 Definition。 |
| `user_visible` | 是否出现在 UI、命令目录或用户可见列表中。 |
| `model_visible` | 是否进入模型可见 Prompt。 |
| `load_policy` | 希望对应子系统何时加载该 Definition。 |

```rust
pub enum LoadPolicy {
    Eager,
    Lazy,
    Manual,
}
```

Turn 对 Prompt 使用完整解析后的设置：

```rust
pub struct EffectiveDefinitionSettings {
    pub enabled: bool,
    pub user_visible: bool,
    pub model_visible: bool,
    pub load_policy: LoadPolicy,
}
```

解析方向：

```text
Runtime defaults
+ Agent overrides
+ Session overrides
→ Turn effective settings
```

安全限制不能简单使用“最后一层覆盖”：

```text
上层明确禁止
→ 下层不能重新启用

上层未指定
→ 下层可以提供更具体设置
```

同一个 PromptDefinition 可以在不同 Session 中具有不同配置：

```text
Session1.prompts.overrides[SystemPrompt].model_visible = Some(true)
Session2.prompts.overrides[SystemPrompt].model_visible = Some(false)
```

这不会修改 `PromptDefinition`，也不会产生新的 `DefinitionVersion`。

## 加载状态

Prompt 用户配置的加载策略与实际加载状态是两个不同概念：

```text
LoadPolicy
→ 用户或 scope 希望何时加载

LoadState
→ 对应子系统实际上是否完成加载
```

```rust
pub enum LoadState {
    Unloaded,
    Loading,
    Loaded,
    Failed {
        message: String,
    },
}
```

实际 LoadState 不进入 Definition，也不进入 `DefinitionOverrides`。PromptService 维护自己的加载记录和 diagnostics。Tool 的注册和 Turn ToolSet snapshot 由 ToolService 维护，不使用该 LoadState；Skill 使用独立的 `SkillLoadState`，由 SkillService 维护。

概念形状：

```rust
pub struct PromptLoadRecord {
    pub prompt_id: PromptId,
    pub version: DefinitionVersion,
    pub state: LoadState,
}
```

加载失败只改变 LoadState 和 diagnostics，不修改 Definition 内容，不产生新 DefinitionVersion。

## 状态模型

### AgentStatus

```rust
pub enum AgentStatus {
    Enabled,
    Disabled,
    Deleted,
}
```

| 状态 | 含义 |
| --- | --- |
| `Enabled` | 允许创建 Session、升级 Session 和 future Turn admission。 |
| `Disabled` | 可读、可编辑 definition，但禁止新的执行使用；可以重新 Enable。 |
| `Deleted` | 不可恢复的逻辑删除；历史 identity 和 immutable revisions 保留。 |

`Deleted` 不等于物理清除；physical `PurgeAgent` 留给未来 retention/admin。

### SessionLifecycle

```rust
pub enum SessionLifecycle {
    Open,
    Archived,
    Deleted,
}
```

| 状态 | 含义 |
| --- | --- |
| `Open` | Durable Session 可以 load 和执行；不表示当前已加载。 |
| `Archived` | 可逆只读状态，可 query/export/fork，但不能执行。 |
| `Deleted` | 不可恢复的逻辑删除；普通 query 默认隐藏。 |

状态机：

```text
Open ↔ Archived
Archived → Deleted
```

`PurgeSession` 才表示物理清除。

### SessionLoadState

```rust
pub enum SessionLoadState {
    Unloaded,
    Loading,
    Loaded,
    Unloading,
}
```

这是进程内 residency projection，不进入 durable Session。进程重启后所有 Session 都视为 Unloaded。

### SessionReadiness

```rust
pub enum SessionReadiness {
    Preparing,
    Ready,
    Unavailable(SessionUnavailable),
}
```

Loaded 不等于 Ready。Workspace、exact AgentRevision 或 conversation 不可用时，history 仍可读，但 Turn admission fail closed。

### SessionExecutionState

```rust
pub enum SessionExecutionState {
    Idle,
    Starting,
    Running,
    Finishing,
}
```

```text
Idle → Starting → Running → Finishing → Idle
Starting failure → Idle
```

这些状态只属于 loaded Session execution，不进入 durable Session。

### TurnStatus

```rust
pub enum TurnStatus {
    Running,
    Completed {
        completed_at: Timestamp,
    },
    Interrupted {
        completed_at: Timestamp,
        reason: TurnInterruption,
    },
    Failed {
        completed_at: Timestamp,
        failure: TurnFailure,
    },
}
```

基础不变量：

- initiating UserMessage entry append 后 Turn 首先处于 `Running`；
- terminal status 是 `Completed | Interrupted | Failed`；
- terminal Turn 不可恢复为 Running；
- 一个 Session 同时最多存在一个 Running Turn；
- WaitingApproval、Sampling 或 ExecutingTools 都只是 Running Turn 的 execution phase。

### TurnExecutionPhase

```rust
pub enum TurnExecutionPhase {
    PreparingModel,
    Compacting,
    Sampling,
    WaitingApproval,
    ExecutingTools,
    Committing,
}
```

等待审批或Compaction时TurnStatus仍为Running。WaitingApproval时InteractionState为Pending且parent ToolInvocationState为Started；Compacting时不创建Item或Interaction。Steer默认排队到当前model/tool/compaction operation完成，不把Turn变为Interrupted。

### ItemStatus

ItemStatus 是从 ItemContent 派生的 read projection，不独立保存：

```rust
pub enum ItemStatus {
    Started,
    Completed,
    Abandoned,
}
```

```text
UserMessage / AgentMessage / Reasoning
→ durable 创建时即 Completed

ToolInvocation
→ Started → Completed | Abandoned
```

Completed 不等于业务成功；failed、denied 和 confirmed cancellation 都可以拥有 truthful ToolResult 并完成。Abandoned 表示没有可安全构造的 ToolResult，不能进入模型 conversation。

### InteractionStatus

InteractionStatus 从 InteractionState 派生，不与 resolution 分开存储：

```rust
pub enum InteractionStatus {
    Pending,
    Resolved,
}
```

```text
Pending { request, timestamps }
→ Resolved { typed resolution, resolved_at }
```

Resolved 只表示 request 已关闭，不表示已批准、成功或 Tool 已执行。

## Turn 边界

Turn 的定义是：

```text
initiating UserMessage entry 成功 append
→ Turn Running
→ final AssistantMessage / TurnInterrupted / TurnFailed entry 成功 append
```

由此得到以下基础不变量：

- 每个 Turn 由一条 initiating UserMessage 开启；
- initiating UserMessage 属于该 Turn 的 Item；
- 下一条普通 UserMessage 必须开启新的 Turn；
- Interaction response 不是 UserMessage，不开启新 Turn；
- approval response 不是 UserMessage，不开启新 Turn；
- ToolResult 不是 UserMessage，不开启新 Turn；
- Turn terminal 后才能开始下一条普通 UserMessage 对应的新 Turn；
- Steer 若继续当前 Turn，必须是 control input，成功 append 后才影响下一次逻辑模型调用；
- FollowUp 只表示“当前 Turn terminal 后提交下一条普通 UserMessage”，因此开启新 Turn 并捕获新 TurnExecutionContext；
- FollowUp 不属于 TurnControl，也不形成独立领域 entity。

如果未来 Steer 被定义为新的 UserMessage，它必须创建新 Turn；不能同时具有“普通 UserMessage”和“继续 current Turn”两种语义。

Standalone compaction、review、background work 等没有 initiating UserMessage 的工作是否属于 Turn，暂不决定。

## 基础身份

本阶段定义以下 identity：

```text
AgentId
AgentRevision
AgentRevisionRef
SessionId
SessionDefinitionRevision
EntryId
OperationKey
TurnId
ItemId
RequestId
ToolCallId
PromptId
SkillId
DefinitionVersion
```

基础路由关系：

```text
Agent.id
AgentDefinition: AgentId + AgentRevision
Session.id + Session.current_revision
SessionDefinition.agent: AgentRevisionRef
StoredSessionEntry: EntryId + parent_id + operation_key
Turn.id + Turn.session_id
Item.id + Item.turn_id
Interaction.request_id + session_id + turn_id + item_id
ToolInvocation Item: ItemId + ToolCallId correlation
Interaction: RequestId + parent ItemId
PromptId + DefinitionVersion
SkillId + DefinitionVersion
```

已经固定：

- `parent_id: Option<EntryId>` 形成 Session 内 entry tree，不建立 BranchId；
- Item/Interaction/Tool operational facts 可以由多个 Message/Event entries 投影；
- fork remap EntryId、TurnId、ItemId、RequestId 和 operation key，以及所有 target-local nested references；
- fork preserve ToolCallId 与 exact content/definition semantics。

仍未定义：

- 各 ID 的具体编码和全局唯一范围；
- ToolCallId 的 provider exact echo adapter；
- physical retention/vacuum 后 EntryId 的长期可达性；
- operation key 的具体编码和 namespace。

## 领域所有权

| 对象 | 领域归属 |
| --- | --- |
| Arc\<PromptService\> / Arc\<ToolService\> / Arc\<SkillService\> 生命周期 | MiniCoreRuntime |
| RuntimePrompts / Prompt source / Prompt load state | PromptService |
| Agent head / current AgentRevision pointer | Agent |
| AgentPrompts / immutable execution definition | AgentDefinition |
| Session head / current SessionDefinitionRevision pointer | Session |
| AgentRevisionRef、SessionPrompts、Workspace definition、Model | SessionDefinition |
| 本 Turn 的有效执行 binding | Turn execution pin 的 TurnExecutionContext |
| 本 Turn 的有效 Prompt snapshot | TurnExecutionContext 内的 PromptSet |
| Runtime Service注入与loaded residency | SessionExecutor |
| 当前 WorkspaceSnapshot | loaded Session execution state；Turn 执行上下文 pin 不可变引用 |
| canonical roots / effective grants / Workspace views | WorkspaceResolver 解析和 WorkspaceSnapshot 投影 |
| persisted trust / managed Workspace policy | WorkspaceAuthority adapter 或其后端 store |
| Session ledger / EntryId / operation key / current leaf | SessionStorage |
| Turn exact Model / Prompt / execution references | TurnContext storage entry |
| Turn | Session |
| Item semantic ownership | Turn；durable projection 来自 SessionStorage |
| Interaction semantic ownership | Item；durable projection 来自 SessionStorage |
| Tool registration、execution policy 和 executor | ToolService |
| 本 Turn 的有效 Tool snapshot | TurnExecutionContext 内的 ToolSet |
| 本 Turn 的 Skill metadata snapshot | TurnExecutionContext 内的 pinned SkillCatalog |
| AgentLoop adapter / logical model-call state / committed conversation hot projection | SessionExecutor |
| Skill load state / cache | Skill 子系统 |

以下问题暂不决定：

- Definition content是内联值还是immutable content reference；
- scope definitions是新增定义、完整集合还是只保存引用；
- 公开Runtime protocol如何联系SessionExecutionHandle。

## 最小代码骨架

```rust
pub struct MiniCoreRuntime {
    pub prompt_service: Arc<PromptService>,
    pub tools: Arc<ToolService>,
    pub skills: Arc<SkillService>,
}

pub struct Agent {
    pub id: AgentId,
    pub current_revision: AgentRevision,
    pub status: AgentStatus,
    pub name: String,
    pub description: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

pub struct AgentDefinition {
    pub agent_id: AgentId,
    pub revision: AgentRevision,
    pub prompts: AgentPrompts,
    pub created_at: Timestamp,
}

pub struct AgentRevisionRef {
    pub agent_id: AgentId,
    pub revision: AgentRevision,
}

pub struct Session {
    pub id: SessionId,
    pub current_revision: SessionDefinitionRevision,
    pub lifecycle: SessionLifecycle,
    pub name: Option<String>,
    pub description: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

pub struct SessionDefinition {
    pub session_id: SessionId,
    pub revision: SessionDefinitionRevision,
    pub agent: AgentRevisionRef,
    pub workspace: Workspace,
    pub model: SessionModelConfig,
    pub prompts: SessionPrompts,
    pub created_at: Timestamp,
}

pub struct Turn {
    pub id: TurnId,
    pub session_id: SessionId,
    pub started_at: Timestamp,
    pub status: TurnStatus,
}

pub struct Item {
    pub id: ItemId,
    pub turn_id: TurnId,
    pub content: ItemContent,
}

pub enum ItemContent {
    UserMessage(UserMessageItem),
    AgentMessage(AgentMessageItem),
    Reasoning(ReasoningItem),
    ToolInvocation(ToolInvocationItem),
}

pub struct Interaction {
    pub request_id: RequestId,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub item_id: ItemId,
    pub request: InteractionRequest,
    pub state: InteractionState,
}
```

## 后续设计顺序

1. command、query、event、snapshot和transport protocol。
2. Rig ModelGateway与AgentLoop adapter implementation spike。
3. SessionExecutor、ModelGateway provider adapter和Compaction生产实现。
4. review、background work和按需Extension/Plugin。

跨阶段 backlog：PromptDefinition priority/content identity、scope merge和Skill namespace/content identity；在对应消费方需要前闭合，不改变上述主路径顺序。

## 设计进度

- [x] 确定 `MiniCoreRuntime → Agent → Session → Turn → Item → Interaction` 基础关系。
- [x] 确定 Agent 可以被多个 Session 引用，一个 Session 只引用一个 Agent。
- [x] 将 Workspace 放在 Session 层，不放在 Agent 或 Turn 层。
- [x] 确定 Workspace 是 Session-owned definition，不建立 WorkspaceService 或 Runtime-global registry。
- [x] 确定当前不定义 WorkspaceId，并定义 WorkspaceRevision、WorkspaceSnapshot 和 WorkspaceFingerprint 的职责。
- [x] 区分 filesystem access、Prompt source authorization 和 Skill source authorization。
- [x] 确定 Turn execution context pin 不可变 WorkspaceSnapshot。
- [x] 不引入通用 Resource 类型或 `RuntimeResources`。
- [x] 将 Prompt 定义为各 scope 下的复数定义集合。
- [x] 为 PromptDefinition 和 SkillMetadata 增加 DefinitionVersion。
- [x] 区分 PromptDefinition、scope overrides 和 runtime LoadState。
- [x] 支持同一个 PromptDefinition 在不同 Session 中具有不同启用和可见状态。
- [x] 固化 Runtime 初始化 PromptService、Turn 执行期创建 PromptSet 的 Prompt 子系统关系。
- [x] 确定 PromptService 只消费 ToolPromptView 和 SkillCatalogView，不主动调用 ToolService 或 SkillService。
- [x] 确定 PromptSet 负责 CanonicalUserMessage、最终模型上下文和 MessageRecord → ModelMessage 转换。
- [x] 确定 Turn 领域对象不持有 PromptSet 或 Prompt fingerprint；exact reference 属于 start execution metadata。
- [x] 确定ModelGateway使用exact TurnModelSnapshot和一个深generate_model_turn operation。
- [x] 确定provider attempt、stream、auth、cache和continuation不进入领域模型。
- [x] 确定 Turn 不持有 Agent identity、revision 或 Workspace。
- [x] 确定 SessionDefinition pin exact AgentRevisionRef，Agent update 不自动改变 Session。
- [x] 定义 AgentDefinition、SessionDefinition 和 SessionDefinitionRevision。
- [x] 区分 Agent/Session durable lifecycle 与 loaded execution state。
- [x] 使用 Enabled/Disabled/Deleted 与 Open/Archived/Deleted。
- [x] 定义 Session load/readiness/execution state 和 TurnExecutionPhase。
- [x] 定义Compaction ownership、stable-unit cut、current UserMessage protection、StoredCompaction和bounded recovery。
- [x] 确定 WaitingApproval 和 Steer 不使 Turn 进入 Interrupted。
- [x] 确定 Turn 从 initiating UserMessage entry append 开始，到 final AssistantMessage、TurnInterrupted 或 TurnFailed entry append 结束。
- [x] 定义 Agent、Session、Turn、Item 和 Interaction 的基础状态。
- [x] 固化 Runtime 初始化 SkillService、loaded Session execution 注入、Turn 按需使用的 Skill 子系统关系。
- [x] 确定 Turn 对象不持有 Skill、Catalog、LoadedSkill 或 Skill snapshot。
- [x] 定义 Skill Catalog 渐进披露、按需加载、cache、失效和 Injection 基础规则。
- [x] 固化 Runtime 初始化 ToolService、Turn 执行期创建 ToolSet 的 Tool 子系统关系。
- [x] 确定 Agent、Session 和 Turn 领域对象不持有 Tool 属性。
- [x] 确定 `ToolService::for_turn(...)` 从 candidate admission 的执行边界开始且不负责创建 Turn。
- [x] 区分领域 Turn、Turn execution 和 AgentLoop 的开始与结束边界。
- [x] 确定 TurnExecutionContext pin WorkspaceSnapshot、SkillCatalog、ToolSet、PromptSet 和 Model。
- [x] 定义逻辑模型调用与 provider retry 边界，不增加 ModelStep 领域类型或独立 ID。
- [x] 区分 Steer 与 FollowUp；FollowUp 开启下一 Turn。
- [x] 定义最小 ItemContent：UserMessage、AgentMessage、Reasoning、ToolInvocation。
- [x] 确定 ItemType/ItemStatus 从 ItemContent 派生，不独立保存。
- [x] 合并 ToolCall/ToolResult 为同一个 ToolInvocation Item。
- [x] 定义 ToolInvocation Started/Completed/Abandoned 和 outcome-unknown recovery。
- [x] 定义 Interaction request/resolution、append-before-notify/wake ordering、reconnect 和 timeout。
- [x] 确定 terminal Turn 不保留 Pending Interaction 或 Started Item。
- [x] 定义 SessionWriter 唯一 append seam、by-entry JSONL tree、Message/Event schema 和 append receipt。
- [x] 定义 CommittedConversationState/Delta/Checkpoint 和 trusted apply。
- [x] 定义 EntryId/parent_id history tree、stable checkpoint 和 fork identity remap。
- [x] 定义 partial tail、strict corruption 和 conservative recovery。
- [ ] 定义 PromptDefinition 的完整字段和内容 identity。
- [ ] 定义 scope 合并规则。
- [ ] 定义 Model。
- [x] 定义 Item type 和 content。
- [x] 定义 Interaction family。
- [x] 定义 Session conversation/storage identity、entry tree、projection 和 recovery。
- [ ] 定义 public protocol。
- [ ] 定义 manager 和 execution ownership。
