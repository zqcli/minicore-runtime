# MiniCore 领域模型

状态：独立领域模型草案
日期：2026-07-16

## 目的

本文从零定义 MiniCore 的目标领域模型，不以任何现有实现、模块划分、协议或存储结构为前提，也不描述从其他架构迁移到本文模型的过程。

本阶段定义以下概念的基础关系、类型、状态和 scope 配置规则：

```text
MiniCoreRuntime
Agent
Session
Turn
Item
Interaction
Prompt
Tool
ToolRuntime
ToolSet
Skill
SkillsService
SkillCatalog
LoadedSkill
SkillInjection
Definition
DefinitionOverrides
LoadState
```

本阶段暂不设计：

- Session、Turn、Prompt 等其他模块的 manager、actor 或 execution owner 最终划分；
- storage、JSONL、database、catalog 或 loaded-runtime registry；
- Workspace 的内部结构；
- Model 的内部结构；
- Prompt 的具体覆盖和内容组装算法；
- Tool 的注册、披露、执行、权限和 Sandbox 实现细节；
- provider、Driver、compaction、retry、Steer 或具体协议 method；
- Item type 的最终封闭集合；
- fork tree 和持久化 identity mapping。

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
- Agent、Session 和 Turn 分别持有自己 scope 下的 Prompt 集合；
- Prompt、Tool、Skill 是独立概念，不合并成通用 `Resource`；
- Prompt 的 scope 类型使用复数名称，明确表示定义集合；
- Definition 保存稳定定义和版本；
- `MiniCoreRuntime` 启动时初始化一个 `Arc<ToolRuntime>`；
- Agent、Session 和 Turn 领域对象不持有 Tool 属性；
- Turn 编排层在 Turn 已开始执行后、第一次模型调用前调用 `ToolRuntime::for_turn(...)`；
- `ToolRuntime::for_turn(...)` 返回执行期不可变 `ToolSet`，同一 Turn 内重复使用；
- `MiniCoreRuntime` 启动时初始化一个 `Arc<SkillsService>`；
- Session 引用 Runtime 创建的同一个 `Arc<SkillsService>`；
- Skill 暂不区分 Runtime、Agent、Session 或 Turn 等配置层级；
- Runtime、Agent、Session 保存各自 scope 的 Prompt 配置或稀疏覆盖；
- Turn 保存解析后的有效 Prompt 集合，不持有 ToolSet 或 Skill；
- Turn 执行期间通过对应 Session 的 SkillsService 查询 Catalog、按需加载 Skill，并交给 Injection 层注入 Prompt；
- 实际加载状态不进入 Definition，由对应子系统单独维护；
- 同一个 PromptDefinition 可以在不同 Session 中具有不同的启用和可见状态；
- Turn 不持有 `AgentId`、`AgentRevision`、`AgentVersionRef` 或 Workspace；
- Turn 从一条用户消息开始，到下一条用户消息开始之前结束；
- Item 是 Turn 内各类消息和可观察内容的统一概念；
- Interaction 是 Item 执行期间产生的 request/response 交互。

## 领域关系

### MiniCoreRuntime

`MiniCoreRuntime` 表示一个完整的 MiniCore runtime 实例，是外部宿主接触 MiniCore 的顶层门面。

它持有 runtime scope 的 Prompt，以及 Runtime 生命周期内唯一的 ToolRuntime 和 SkillsService：

```rust
pub struct MiniCoreRuntime {
    pub prompts: RuntimePrompts,
    pub tools: Arc<ToolRuntime>,
    pub skills: Arc<SkillsService>,
}
```

ToolRuntime 的完整设计见 [Tool 子系统架构设计](tool-subsystem.md)。

SkillsService 的完整设计见 [Skill 子系统架构设计](skill-subsystem.md)。

本阶段不定义 `MiniCoreRuntime` 的 command、query、event、snapshot interface，也不定义它如何创建、保存或查找 Agent。

### Agent

Agent 表示可被一个或多个 Session 引用的 Agent 对象。

```rust
pub struct Agent {
    pub id: AgentId,
    pub revision: AgentRevision,
    pub status: AgentStatus,
    pub name: String,
    pub description: Option<String>,
    pub prompts: AgentPrompts,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}
```

Agent 的 `name` 是用户可见名称，不承担稳定 identity；`AgentId` 才是稳定 identity。

`description` 只表达 Agent 的用户可见说明，不因为字段存在而自动进入模型上下文。

`AgentRevision` 表示会影响 Agent 执行定义的版本：

```text
修改 AgentPrompts
→ AgentRevision 改变

只修改 name / description
→ AgentRevision 不改变
→ updated_at 改变
```

Agent 不持有：

- Workspace；
- Session conversation；
- current Turn；
- Item；
- Interaction；
- provider client 或 credentials；
- manager、registry 或 storage handle。

### Session

Session 是标准对话对象。它把一个 Agent 与一个具体 Workspace、Model 和 Prompt 上下文关联起来，并引用 Runtime 初始化的 SkillsService。

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

Session 只允许引用一个 Agent：

```text
Session.agent_id → Agent.id
```

Session 的 `name` 可以为空，因为新对话创建时可能尚未生成用户可见标题。

Session 不持有 Agent revision。Agent revision 如何影响已经存在的 Session 和 future Turn，暂不决定。

Session 不复制 Skill definitions、Skill references、Skill Catalog 或完整 Skill 内容。它持有 Runtime 创建的同一个 `Arc<SkillsService>`，并通过该服务建立或获取轻量 Catalog。完整 Skill 内容在 Turn 执行期间按需加载。详细规则见 [Skill 子系统架构设计](skill-subsystem.md)。

`Workspace` 和 `Model` 在本文中都是不透明类型：

```rust
pub struct Workspace;
pub struct Model;
```

它们的 identity、字段、状态和生命周期将在后续设计中单独确定。

### Turn

Turn 表示由一条用户消息开启、在下一条用户消息开始前结束的过程。

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

Turn 不直接持有 Agent identity 或 Workspace：

```text
Turn.session_id
→ Session.agent_id
→ Agent.id

Turn.session_id
→ Session.workspace
```

Turn 不包含以下字段：

```text
agent_id
agent_revision
agent_version_ref
agent_snapshot
workspace
```

Turn scope 的 Model 暂时使用不透明类型：

```rust
pub struct TurnModel;
```

Turn 持有已经解析完成的 `TurnPrompts`。它不再保存 Runtime、Agent、Session 的稀疏覆盖规则。

Turn 领域对象不持有 Tool、ToolSet、ToolSpec 或 executor。Turn 编排层在 Turn 已创建并进入执行阶段后、第一次模型调用前，通过 Runtime 的 `ToolRuntime::for_turn(...)` 创建执行期 `ToolSet`。同一 Turn 内的全部 LLM → Tool → LLM 循环复用该 ToolSet，Turn terminal 后释放。`for_turn` 不创建 Turn，也不修改 TurnStatus。完整规则见 [Tool 子系统架构设计](tool-subsystem.md)。

Turn 不持有 Skill、Skill Catalog、LoadedSkill 或 Skill snapshot。Turn 执行期间若确定需要某个 Skill，Turn 编排层通过对应 Session 的 SkillsService 按需加载，再由 Injection 层转换为本轮 Prompt contribution。加载结果不写回 Turn 对象。

### Item

Item 是 Turn 内各类消息和可观察内容的统一概念。

```rust
pub struct Item {
    pub id: ItemId,
    pub turn_id: TurnId,
    pub item_type: ItemType,
    pub status: ItemStatus,
    pub content: ItemContent,
}
```

以下对象属于 Item 的语义范围：

```text
UserMessage
AgentMessage
Reasoning
ToolCall
ToolResult
```

是否加入 Plan、FileChange、CommandExecution、Compaction 或其他 Item type，留待后续 Item 设计决定。

`ItemType` 和 `ItemContent` 暂时保持不透明：

```rust
pub struct ItemType;
pub struct ItemContent;
```

### Interaction

Interaction 表示某个 Item 执行期间，由 Runtime 发起并等待外部回答的 request/response 交互。

```rust
pub struct Interaction {
    pub request_id: RequestId,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub item_id: ItemId,
    pub status: InteractionStatus,
    pub request: InteractionRequest,
    pub resolution: Option<InteractionResolution>,
}
```

Interaction 的归属关系固定为：

```text
Interaction → Item → Turn → Session → Agent
```

Approval、用户补充输入和其他外部请求都可以在未来定义为 Interaction type。具体 request family、decision set、timeout、恢复和 transport 行为暂不决定。

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

## Prompt 类型体系

`Prompt` 表示一组 PromptDefinition，不表示一条单独 Prompt。

领域继承关系：

```text
Prompt
├─ RuntimePrompts
├─ AgentPrompts
├─ SessionPrompts
└─ TurnPrompts
```

具体定义：

```rust
pub struct PromptDefinition {
    pub id: PromptId,
    pub version: DefinitionVersion,
    pub key: PromptKey,
    pub name: String,
    pub description: Option<String>,
    pub content: PromptContent,
}
```

其中：

- `PromptId` 是稳定 identity；
- `PromptKey` 是 scope 内机器可读的稳定 key；
- `name` 是用户可见名称；
- `description` 不自动进入模型上下文；
- `content` 是 Prompt 定义内容。

集合形状：

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

pub struct TurnPrompts {
    pub definitions: Vec<EffectivePromptDefinition>,
}
```

Prompt 使用 `Vec` 作为权威定义集合，因为 Prompt 顺序可能影响最终模型上下文。`PromptKey` 在同一 scope 内必须唯一。

`EffectivePromptDefinition` 表示已经解析 scope defaults/overrides 后进入 Turn 的 Prompt 定义：

```rust
pub struct EffectivePromptDefinition {
    pub definition: PromptDefinition,
    pub settings: EffectiveDefinitionSettings,
}
```

Prompt 的最终内容组装规则暂不决定。

## Tool 子系统引用

Tool 使用独立的 ToolRuntime 架构，不建立 `RuntimeTools / AgentTools / SessionTools / TurnTools` 领域分层。

基础关系：

```text
MiniCoreRuntime
└─ Arc<ToolRuntime>

Turn execution orchestration
└─ ToolRuntime::for_turn(ToolTurnContext)
   └─ ToolSet
      ├─ specs() → 本 Turn 模型可见 ToolSpec
      └─ execute(ToolCall[]) → ToolResult[]
```

`ToolRuntime::for_turn(...)` 从 Turn 执行边界开始：Turn 已经创建并进入执行阶段，由 Turn 编排层在第一次模型调用前调用。该方法不创建领域 Turn，不改变 TurnStatus，也不属于 Turn 对象。返回的 ToolSet 只存在于执行期，不写入 Agent、Session 或 Turn。

领域投影固定为：

```text
ToolCall
→ Item

ToolResult
→ Item

Tool approval request / decision
→ Interaction，归属于对应 ToolCall Item
```

Tool 注册、ToolSpec、Direct/Deferred/Hidden 披露、参数校验、Hook、policy、approval、Sandbox、并发和稳定结果顺序以 [Tool 子系统架构设计](tool-subsystem.md) 为权威。

## Skill 子系统引用

Skill 使用独立的 SkillsService 架构，不建立 `RuntimeSkills / AgentSkills / SessionSkills / TurnSkills` 领域分层。

基础关系：

```text
MiniCoreRuntime
└─ Arc<SkillsService>

Session
└─ Arc<SkillsService>   // 引用 Runtime 创建的同一个 service

Turn execution
└─ 通过 Session.skills 查询 Catalog 或按需加载 Skill
   └─ SkillInjector
      └─ PromptContribution
```

Skill Catalog 只包含名称、描述、路径、作用域和内容 identity 等轻量 metadata。完整 Skill 内容由 SkillsService 在 Turn 执行期间确定需要后按需加载、解析并缓存。

Turn 对象不持有 Skill，也不保存 Catalog、LoadedSkill 或 Skill snapshot。SkillsService 不决定哪个 Turn 使用哪个 Skill；该决定属于 Turn 编排层。Injection 层只负责把已加载内容转换为本轮 Prompt contribution。

Skill 子系统的对象、interface、渐进披露、cache、失效和 diagnostics 规则以 [Skill 子系统架构设计](skill-subsystem.md) 为权威。

## Scope 配置

本节的 scope 配置只适用于 Prompt。Tool 的本 Turn 选择和披露由 `ToolRuntime::for_turn(...)` 根据执行上下文完成，见 [Tool 子系统架构设计](tool-subsystem.md)。Skill 当前不建立 Runtime、Agent、Session 或 Turn 配置层级，其过滤和加载规则见 [Skill 子系统架构设计](skill-subsystem.md)。

PromptDefinition 本身不保存某个 Agent、Session 或 Turn 的启用、可见或加载状态。

Runtime、Agent 和 Session 分别保存各自 scope 的 defaults/overrides：

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

实际 LoadState 不进入 Definition，也不进入 `DefinitionOverrides`。Prompt 子系统维护自己的加载记录和 diagnostics。Tool 的注册和 Turn ToolSet snapshot 由 ToolRuntime 维护，不使用该 LoadState；Skill 使用独立的 `SkillLoadState`，由 SkillsService 维护。

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
    Tombstoned,
}
```

| 状态 | 含义 |
| --- | --- |
| `Enabled` | Agent 可以被正常引用。 |
| `Disabled` | Agent 仍然存在，但新的使用行为应被限制；精确限制后续决定。 |
| `Tombstoned` | Agent 已被逻辑删除；历史引用是否继续可读后续决定。 |

### SessionStatus

```rust
pub enum SessionStatus {
    NotLoaded,
    Idle,
    Active,
    SystemError,
}
```

| 状态 | 含义 |
| --- | --- |
| `NotLoaded` | Session 存在，但没有 loaded execution state。 |
| `Idle` | Session 已加载，当前没有 active Turn。 |
| `Active` | Session 已加载，并且当前存在 active Turn。 |
| `SystemError` | Session 无法安全继续普通执行。 |

本阶段不定义 active flags、approval flags、retry phase 或 compaction phase。

### TurnStatus

```rust
pub enum TurnStatus {
    InProgress,
    Completed,
    Interrupted,
    Failed,
}
```

基础不变量：

- Turn 创建后首先处于 `InProgress`；
- Turn 必须到达一个 terminal status；
- terminal status 是 `Completed | Interrupted | Failed`；
- terminal Turn 不可恢复为 `InProgress`；
- 一个 Session 同时最多存在一个 `InProgress` Turn。

### ItemStatus

```rust
pub enum ItemStatus {
    Started,
    Completed,
}
```

Item 的通用生命周期为：

```text
Started
→ 可选 typed delta
→ Completed
```

Item-specific success、failure、declined、cancelled 等结果是否进入 `ItemStatus`，还是进入 typed `ItemContent`，暂不决定。

### InteractionStatus

```rust
pub enum InteractionStatus {
    Pending,
    Resolved,
}
```

基础生命周期：

```text
Pending
→ 可选外部 response
→ Resolved
```

`Resolved` 只表示 request 已关闭，不表示已批准、成功或执行完成。

## Turn 边界

Turn 的定义是：

```text
从一条用户消息开始
到下一条用户消息开始之前结束
```

由此得到以下基础不变量：

- 每个 Turn 由一条 initiating UserMessage 开启；
- initiating UserMessage 属于该 Turn 的 Item；
- 下一条普通 UserMessage 必须开启新的 Turn；
- Interaction response 不是 UserMessage，不开启新 Turn；
- approval response 不是 UserMessage，不开启新 Turn；
- ToolResult 不是 UserMessage，不开启新 Turn；
- Turn terminal 后才能开始下一条普通 UserMessage 对应的新 Turn。

如果未来 Steer 被定义为新的 UserMessage，它必须创建新 Turn；如果 Steer 需要继续当前 Turn，它必须被定义为 control input，而不能同时被定义为 UserMessage。

Standalone compaction、review、background work 等没有 initiating UserMessage 的工作是否属于 Turn，暂不决定。

## 基础身份

本阶段定义以下 identity：

```text
AgentId
AgentRevision
SessionId
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
Session.id + Session.agent_id
Turn.id + Turn.session_id
Item.id + Item.turn_id
Interaction.request_id + session_id + turn_id + item_id
TurnId + ToolCallId
PromptId + DefinitionVersion
SkillId + DefinitionVersion
```

本阶段不定义：

- fork tree identity；
- storage entry identity；
- ToolCallId 的生成方式、全局唯一范围和 provider call ID mapping；
- ID 的生成方式；
- ID 的全局唯一范围；
- fork 时 ID 是否保留；
- Item 与持久化记录的映射。

## 领域所有权

| 对象 | 领域归属 |
| --- | --- |
| RuntimePrompts / Arc\<ToolRuntime\> / Arc\<SkillsService\> 生命周期 | MiniCoreRuntime |
| AgentPrompts | Agent |
| SessionPrompts / Arc\<SkillsService\> 引用 | Session |
| TurnPrompts | Turn |
| Workspace / Model | Session |
| TurnModel | Turn |
| Turn | Session |
| Item | Turn |
| Interaction | Item |
| Prompt load state | Prompt 子系统 |
| Tool registration、execution policy 和 executor | ToolRuntime |
| 本 Turn 的有效 Tool snapshot | Turn 执行期局部 ToolSet |
| Skill load state / cache | Skill 子系统 |

以下问题暂不决定：

- 哪个 actor 或 manager 持有 mutable state；
- Definition content 是内联值还是 immutable content reference；
- scope definitions 是新增定义、完整集合还是只保存引用；
- Item 和 Interaction 是否持久化；
- Runtime 是否允许多个 Agent 或 Session 同时 active。

## 最小代码骨架

```rust
pub struct MiniCoreRuntime {
    pub prompts: RuntimePrompts,
    pub tools: Arc<ToolRuntime>,
    pub skills: Arc<SkillsService>,
}

pub struct Agent {
    pub id: AgentId,
    pub revision: AgentRevision,
    pub status: AgentStatus,
    pub name: String,
    pub description: Option<String>,
    pub prompts: AgentPrompts,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

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

pub struct Item {
    pub id: ItemId,
    pub turn_id: TurnId,
    pub item_type: ItemType,
    pub status: ItemStatus,
    pub content: ItemContent,
}

pub struct Interaction {
    pub request_id: RequestId,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub item_id: ItemId,
    pub status: InteractionStatus,
    pub request: InteractionRequest,
    pub resolution: Option<InteractionResolution>,
}
```

## 后续设计顺序

1. PromptDefinition 的完整字段和内容 identity。
2. Runtime、Agent、Session scope Prompt definitions 与 overrides 的精确合并规则。
3. 继续完善 SkillMetadata、Skill content identity 和 Injection 格式。
4. Prompt 子系统的加载、cache、diagnostics 和 snapshot interface。
5. Workspace 和 Model 的最小类型。
6. Turn 的 admission、terminal 和“下一条用户消息”边界。
7. Item type、Item content 和 Item lifecycle。
8. Interaction request family、response 和 resolution。
9. Agent revision 对 Session 和 Turn 的影响。
10. Session conversation、storage 和 reload。
11. fork identity 和历史树。
12. manager、actor、registry 和 execution ownership。
13. command、query、event、snapshot 和 transport protocol。
14. Prompt 组装、Tool 执行和 Skill invocation。
15. retry、compaction、Steer、review 和 background work。

## 设计进度

- [x] 确定 `MiniCoreRuntime → Agent → Session → Turn → Item → Interaction` 基础关系。
- [x] 确定 Agent 可以被多个 Session 引用，一个 Session 只引用一个 Agent。
- [x] 将 Workspace 放在 Session 层，不放在 Agent 或 Turn 层。
- [x] 不引入通用 Resource 类型或 `RuntimeResources`。
- [x] 将 Prompt 定义为各 scope 下的复数定义集合。
- [x] 为 PromptDefinition 和 SkillMetadata 增加 DefinitionVersion。
- [x] 区分 PromptDefinition、scope overrides 和 runtime LoadState。
- [x] 支持同一个 PromptDefinition 在不同 Session 中具有不同启用和可见状态。
- [x] 确定 Turn 不持有 Agent identity、revision 或 Workspace。
- [x] 确定 Turn 从一条用户消息开始，到下一条用户消息开始前结束。
- [x] 定义 Agent、Session、Turn、Item 和 Interaction 的基础状态。
- [x] 固化 Runtime 初始化 SkillsService、Session 引用、Turn 按需使用的 Skill 子系统关系。
- [x] 确定 Turn 对象不持有 Skill、Catalog、LoadedSkill 或 Skill snapshot。
- [x] 定义 Skill Catalog 渐进披露、按需加载、cache、失效和 Injection 基础规则。
- [x] 固化 Runtime 初始化 ToolRuntime、Turn 执行期创建 ToolSet 的 Tool 子系统关系。
- [x] 确定 Agent、Session 和 Turn 领域对象不持有 Tool 属性。
- [x] 确定 `ToolRuntime::for_turn(...)` 从 Turn 执行边界开始且不负责创建 Turn。
- [ ] 定义 PromptDefinition 的完整字段和内容 identity。
- [ ] 定义 scope 合并规则。
- [ ] 定义 Workspace 和 Model。
- [ ] 定义 Item type 和 content。
- [ ] 定义 Interaction family。
- [ ] 定义 storage 和 protocol。
- [ ] 定义 manager 和 execution ownership。
