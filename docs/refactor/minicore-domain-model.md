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
Skill
Definition
DefinitionOverrides
LoadState
```

本阶段暂不设计：

- manager、registry、actor 或 execution owner 的最终划分；
- storage、JSONL、database、catalog 或 loaded-runtime registry；
- Workspace 的内部结构；
- Model 的内部结构；
- Prompt、Tool、Skill 的具体覆盖和内容组装算法；
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
- Agent、Session 和 Turn 分别持有自己 scope 下的 Prompt、Tool 和 Skill 集合；
- Prompt、Tool、Skill 是独立概念，不合并成通用 `Resource`；
- Prompt、Tool、Skill 的 scope 类型都使用复数名称，明确表示定义集合；
- Definition 保存稳定定义和版本；
- Skill 的来源和生效层级是两个正交维度：`SkillSource` 描述定义来源，Runtime/Agent/Session 描述配置生效层级；
- 该 Skill 双维度模型是后续完善的暂定方向，不是最终方案；
- Runtime、Agent、Session 保存各自 scope 的配置或稀疏覆盖；
- Turn 保存解析后的有效 Prompt、Tool 和 Skill 集合；
- 实际加载状态不进入 Definition，由对应子系统单独维护；
- 同一个 Definition 可以在不同 Session 中具有不同的启用和可见状态；
- Turn 不持有 `AgentId`、`AgentRevision`、`AgentVersionRef` 或 Workspace；
- Turn 从一条用户消息开始，到下一条用户消息开始之前结束；
- Item 是 Turn 内各类消息和可观察内容的统一概念；
- Interaction 是 Item 执行期间产生的 request/response 交互。

## 领域关系

### MiniCoreRuntime

`MiniCoreRuntime` 表示一个完整的 MiniCore runtime 实例，是外部宿主接触 MiniCore 的顶层门面。

它持有 runtime scope 的 Prompt、Tool 和 Skill 集合：

```rust
pub struct MiniCoreRuntime {
    pub prompts: RuntimePrompts,
    pub tools: RuntimeTools,
    pub skills: RuntimeSkills,
}
```

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
    pub tools: AgentTools,
    pub skills: AgentSkills,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}
```

Agent 的 `name` 是用户可见名称，不承担稳定 identity；`AgentId` 才是稳定 identity。

`description` 只表达 Agent 的用户可见说明，不因为字段存在而自动进入模型上下文。

`AgentRevision` 表示会影响 Agent 执行定义的版本：

```text
修改 AgentPrompts / AgentTools / AgentSkills
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

Session 是标准对话对象。它把一个 Agent 与一个具体 Workspace、Model、Prompt、Tool 和 Skill 上下文关联起来。

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
    pub tools: SessionTools,
    pub skills: SessionSkills,
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
    pub tools: TurnTools,
    pub skills: TurnSkills,
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

Turn 持有已经解析完成的 `TurnPrompts`、`TurnTools` 和 `TurnSkills`。它们不再保存 Runtime、Agent、Session 的稀疏覆盖规则。

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

Prompt、Tool 和 Skill 都由 versioned Definition 表达具体定义。

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

## Tool 类型体系

`Tool` 表示一组 ToolDefinition，不表示一次具体工具调用。

领域继承关系：

```text
Tool
├─ RuntimeTools
├─ AgentTools
├─ SessionTools
└─ TurnTools
```

具体定义：

```rust
pub struct ToolDefinition {
    pub id: ToolId,
    pub version: DefinitionVersion,
    pub name: ToolName,
    pub display_name: String,
    pub description: String,
    pub input_schema: ToolInputSchema,
}
```

集合形状：

```rust
pub struct RuntimeTools {
    pub definitions: Vec<ToolDefinition>,
    pub defaults: HashMap<ToolId, DefinitionOverrides>,
}

pub struct AgentTools {
    pub definitions: Vec<ToolDefinition>,
    pub overrides: HashMap<ToolId, DefinitionOverrides>,
}

pub struct SessionTools {
    pub definitions: Vec<ToolDefinition>,
    pub overrides: HashMap<ToolId, DefinitionOverrides>,
}

pub struct TurnTools {
    pub definitions: Vec<EffectiveToolDefinition>,
}
```

Tool definitions 使用 `Vec` 保存确定顺序，并要求 `ToolName` 在同一 scope 内唯一。按名称执行时需要的索引属于可重建运行时结构：

```rust
pub struct ToolRegistry {
    executors: HashMap<ToolName, ToolExecutor>,
}
```

两者含义不同：

```text
RuntimeTools / AgentTools / SessionTools / TurnTools
→ Tool definitions 和 scope 配置

ToolRegistry
→ ToolName 到真实 executor 的运行时索引

ToolCall
→ Item

ToolResult
→ Item

Tool approval
→ Interaction
```

ToolRegistry、executor 生命周期和 schema/executor 一致性将在 Tool 子系统设计中确定。

## Skill 类型体系

`Skill` 表示一组 SkillDefinition，不表示一次 Skill invocation。

领域继承关系：

```text
Skill
├─ RuntimeSkills
├─ AgentSkills
├─ SessionSkills
└─ TurnSkills
```

具体定义：

```rust
pub struct SkillDefinition {
    pub id: SkillId,
    pub version: DefinitionVersion,
    pub source: SkillSource,
    pub name: SkillName,
    pub display_name: String,
    pub description: String,
    pub content: SkillContent,
}
```

集合形状：

```rust
pub struct RuntimeSkills {
    pub definitions: Vec<SkillDefinition>,
    pub defaults: HashMap<SkillId, DefinitionOverrides>,
}

pub struct AgentSkills {
    pub definitions: Vec<SkillDefinition>,
    pub overrides: HashMap<SkillId, DefinitionOverrides>,
}

pub struct SessionSkills {
    pub definitions: Vec<SkillDefinition>,
    pub overrides: HashMap<SkillId, DefinitionOverrides>,
}

pub struct TurnSkills {
    pub definitions: Vec<EffectiveSkillDefinition>,
}
```

Skill definitions 使用 `Vec` 保存确定顺序，并要求 `SkillName` 在同一 scope 内唯一。名称、路径或 invocation lookup index 是可重建投影，不是第二份 source of truth。

Skill invocation、Skill content 展开和相关 Item 表达暂不决定。

### Codex Skill 管理参考

Codex 的 Skill 管理模式可以作为后续实现参考，但不构成 MiniCore 的兼容要求：

```text
SkillMetadata      → 定义
SkillConfigRules   → scope 配置
SkillsService      → 加载/cache
TurnSkillsContext  → 当前 Turn 快照
```

Codex 中：

- Skill definition 保存在 `SkillMetadata`；
- 启用状态由不同配置层的 rule 决定；
- `SkillsService` 负责发现、加载、不可变 snapshot 和 cache；
- Turn 最终捕获 `HostSkillsSnapshot`，并通过 `TurnSkillsContext` 使用；
- Definition、scope 配置、加载状态和 Turn 快照保持分离。

该模式支持 MiniCore 当前的基础方向：

```text
SkillDefinition
→ Runtime/Agent/Session scope defaults or overrides
→ Skill subsystem load/cache
→ TurnSkills effective snapshot
```

### Skill 分层暂定方向

本节记录当前沟通结论，供后续继续完善。它不是最终 Skill 方案，也不提前锁定 storage、loader、manager 或 protocol 实现。

Codex 的 `SkillScope` 更接近“Skill 从哪里来”；MiniCore 的 Runtime/Agent/Session/Turn 分层更接近“Skill 在哪里配置和生效”。两者解决不同问题，不应共用同一个 scope 概念。

暂定使用两个正交维度：

```text
SkillSource
→ Definition 来源

Runtime / Agent / Session settings
→ Definition 在哪个层级被配置

TurnSkills
→ 当前 Turn 的最终有效快照
```

Skill 来源暂定形状：

```rust
pub enum SkillSource {
    BuiltIn,
    User,
    Workspace,
    Plugin {
        plugin_id: PluginId,
    },
    Agent {
        agent_id: AgentId,
    },
    Session {
        session_id: SessionId,
    },
}
```

`SkillSource` 进入 `SkillDefinition`，只描述来源和 provenance，不决定当前是否启用、是否对用户可见、是否对模型可见或是否已经加载。

MiniCore 的分层职责暂定为：

```text
RuntimeSkills
→ runtime 全局可发现 Skill definitions 和默认配置

AgentSkills
→ Agent 自有 Skill definitions，以及 Agent scope overrides

SessionSkills
→ Session 自有 Skill definitions，以及 Session scope overrides

TurnSkills
→ Runtime + Agent + Session 解析后的有效 Skill snapshot
```

Turn 不是 Skill 管理层：

- Turn 不修改 SkillDefinition；
- Turn 不保存 Runtime、Agent、Session 的 override 规则；
- Turn 只持有本次执行使用的 `EffectiveSkillDefinition` 集合；
- Skill 的加载、cache、diagnostics 和 snapshot 由 Skill 子系统负责。

暂定完整路径：

```text
SkillSource
→ SkillDefinition
→ Runtime/Agent/Session defaults or overrides
→ Skill subsystem discovery/load/cache
→ TurnSkills effective snapshot
→ Skill invocation
```

该方向后续仍需验证：

- `Agent` 和 `Session` 是否允许创建自己的 SkillDefinition，还是只允许引用和覆盖已有定义；
- SkillSource 的优先级、同名冲突和 namespace 规则；
- Workspace Skill 与 Session Skill 的关系；
- TurnSkills snapshot 的 identity、内容冻结和 reload 规则；
- disabled、user-visible、model-visible 和 load-policy 的精确合并语义。

## Scope 配置

Definition 本身不保存某个 Agent、Session 或 Turn 的启用、可见或加载状态。

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
| `model_visible` | 是否进入模型可见 Prompt、Tool schema 或 Skill catalog。 |
| `load_policy` | 希望对应子系统何时加载该 Definition。 |

```rust
pub enum LoadPolicy {
    Eager,
    Lazy,
    Manual,
}
```

Turn 使用完整解析后的设置：

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

同一个 Definition 可以在不同 Session 中具有不同配置：

```text
Session1.tools.overrides[ReadTool].model_visible = Some(true)
Session2.tools.overrides[ReadTool].model_visible = Some(false)
```

这不会修改 `ToolDefinition`，也不会产生新的 `DefinitionVersion`。

## 加载状态

用户配置的加载策略与实际加载状态是两个不同概念：

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

实际 LoadState 不进入 Definition，也不进入 `DefinitionOverrides`。Prompt、Tool、Skill 各自的子系统维护自己的加载记录和 diagnostics。

概念形状：

```rust
pub struct PromptLoadRecord {
    pub prompt_id: PromptId,
    pub version: DefinitionVersion,
    pub state: LoadState,
}

pub struct ToolLoadRecord {
    pub tool_id: ToolId,
    pub version: DefinitionVersion,
    pub state: LoadState,
}

pub struct SkillLoadRecord {
    pub skill_id: SkillId,
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
PromptId
ToolId
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
PromptId + DefinitionVersion
ToolId + DefinitionVersion
SkillId + DefinitionVersion
```

本阶段不定义：

- fork tree identity；
- storage entry identity；
- tool call identity；
- ID 的生成方式；
- ID 的全局唯一范围；
- fork 时 ID 是否保留；
- Item 与持久化记录的映射。

## 领域所有权

| 对象 | 领域归属 |
| --- | --- |
| RuntimePrompts / RuntimeTools / RuntimeSkills | MiniCoreRuntime |
| AgentPrompts / AgentTools / AgentSkills | Agent |
| SessionPrompts / SessionTools / SessionSkills | Session |
| TurnPrompts / TurnTools / TurnSkills | Turn |
| Workspace / Model | Session |
| TurnModel | Turn |
| Turn | Session |
| Item | Turn |
| Interaction | Item |
| Prompt load state | Prompt 子系统 |
| Tool load state / executor registry | Tool 子系统 |
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
    pub tools: RuntimeTools,
    pub skills: RuntimeSkills,
}

pub struct Agent {
    pub id: AgentId,
    pub revision: AgentRevision,
    pub status: AgentStatus,
    pub name: String,
    pub description: Option<String>,
    pub prompts: AgentPrompts,
    pub tools: AgentTools,
    pub skills: AgentSkills,
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
    pub tools: SessionTools,
    pub skills: SessionSkills,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

pub struct Turn {
    pub id: TurnId,
    pub session_id: SessionId,
    pub status: TurnStatus,
    pub model: TurnModel,
    pub prompts: TurnPrompts,
    pub tools: TurnTools,
    pub skills: TurnSkills,
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

1. PromptDefinition、ToolDefinition、SkillDefinition 的完整字段和内容 identity。
2. 完善 SkillSource 与 Runtime/Agent/Session 生效层级的双维度模型。
3. Runtime、Agent、Session scope definitions 与 overrides 的精确合并规则。
4. Prompt、Tool、Skill 子系统的加载、cache、diagnostics 和 snapshot interface。
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
- [x] 将 Prompt、Tool、Skill 定义为各 scope 下的复数定义集合。
- [x] 为 PromptDefinition、ToolDefinition、SkillDefinition 增加 DefinitionVersion。
- [x] 区分 Definition、scope overrides 和 runtime LoadState。
- [x] 支持同一个 Definition 在不同 Session 中具有不同启用和可见状态。
- [x] 确定 Turn 不持有 Agent identity、revision 或 Workspace。
- [x] 确定 Turn 从一条用户消息开始，到下一条用户消息开始前结束。
- [x] 定义 Agent、Session、Turn、Item 和 Interaction 的基础状态。
- [x] 记录 Codex Skill 管理模式作为参考。
- [x] 记录 `SkillSource` 与 Runtime/Agent/Session 生效层级的暂定双维度模型。
- [ ] 定义三类 Definition 的完整字段和内容 identity。
- [ ] 定义 scope 合并规则。
- [ ] 定义 Workspace 和 Model。
- [ ] 定义 Item type 和 content。
- [ ] 定义 Interaction family。
- [ ] 定义 storage 和 protocol。
- [ ] 定义 manager 和 execution ownership。
