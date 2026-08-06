# Agent 与 Session 生命周期架构设计

状态：当前权威架构（ADR 0136/0137；M5.0 recovery/root lease/owner-tracked actor、private reservation foundation、crate-private Agent Create与ordinary Session Create exact G1 publication、unloaded RecordedHistory + Genesis Session Fork tracer、Agent status/definition/metadata、Session metadata CAS、Session definition/Agent revision upgrade CAS、Session lifecycle existing-head action tracer，以及exact historical Agent/Session definition resolution已实现；public Runtime command接入、remaining Fork anchors/LiveSnapshot、loaded Workspace Idle/Snapshot publication、Session lifecycle Runtime residency integration、完整cross-platform native matrix、Recorder/replay pending）
日期：2026-07-31

## 目的

本文定义 MiniCore Agent 与 Session 的 durable lifecycle、definition revision、精确引用、load/unload、execution readiness、fork、并发线性化和 crash recovery 语义。

本文重点解决：

- Agent 是可复用定义还是运行对象；
- Agent 更新如何影响已有 Session、active Turn 和 future Turn；
- Session 是否跟随 Agent current revision；
- durable Session lifecycle 与 loaded execution state 如何分离；
- `SessionDefinitionRevision` 如何成为 Turn admission 的原子配置基础；
- create、update、disable、archive、delete、fork、load 和 unload 的精确边界；
- host restart 后恢复什么，哪些运行时状态必须丢弃。

本文不提前冻结：

- DurableState physical store、root lease、immutable generation和publication由[DurableState](durable-state.md)与[Durable Store V1](../formats/durable-store-v1.md)冻结；本模块不复制其实现接口；
- Runtime command、query、event 和 transport payload；
- Conversation Storage的Header/JSONL entry tree、replay和fork semantic seed；这些以 [Conversation Recording 与 Replay](conversation-storage.md) 为权威；
- loaded Session的SessionExecutor具体scheduler/runtime实现；其ownership以[Session Execution架构设计](session-execution.md)为权威；
- Agent definition 的 Tool/Skill/Model 等未来完整字段；
- physical purge、retention 和 revision GC 策略。

## 同类项目结论

| 项目 | 生命周期形状 | 值得借鉴 | 需要避免 |
| --- | --- | --- | --- |
| Codex | durable ThreadStore 与 loaded ThreadManager 分离；start/resume/fork/archive/unarchive；idle unload | durable Session 与 live execution 分离；fork 使用稳定 Turn 边界；archive 与 resume 清楚 | 没有独立 Agent revision；resume override 与 loaded config 合并复杂 |
| pi | Agent 是内存执行对象；AgentSession 连接执行、JSONL 和配置；switch/fork 替换 runtime | Agent loop 与 session persistence 分离；branch/fork 简单 | model/tools/system prompt 可以在后续 sampling 中刷新，旧 Session 行为容易漂移 |
| Grok Build | declarative AgentDefinition 构建 per-session Agent；durable session 与 SessionActor 分离 | Agent definition 独立；live state 明确；durable-first recovery | SessionActor 过大；model/agent rebuild 和 fork copy 复杂 |
| Claude Code | declarative subagent；Session resume/fork/clear；settings 和 definitions 可热更新 | 用户可理解的 resume/fork/checkpoint；定义与执行分开 | 没有公开 exact definition revision pinning，旧 Session 后续行为可能变化 |
| Cursor | rules、chat、checkpoint 和 background/cloud agents | 执行隔离和 checkpoint 产品体验 | 内部生命周期未公开，不能据此推断 core ownership |

MiniCore 采用 Codex/Grok 的 durable/live 分离，但比这些项目更严格地保存 exact Agent 和 Session definition revision。

## 决策摘要

已经确定：

- Agent 是 Runtime-owned、可被多个 Session 引用的 durable entity；
- Agent 不具有 loaded/unloaded lifecycle；
- Agent execution definition 使用不可变 `AgentRevision`；
- Session definition pin 精确 `AgentRevisionRef`，不自动跟随 Agent current revision；
- Agent 更新只产生“新 revision 可用”，既有 Session 必须显式升级；
- 一个 Session 只能引用一个 AgentId；普通 update 不能把 Session 改绑到另一个 Agent；
- Session definition 使用不可变 `SessionDefinitionRevision`；
- SessionDefinition原子绑定AgentRevisionRef、Workspace、SessionModelConfig和SessionPromptSelection；
- Session durable lifecycle 使用 `Open / Archived / Deleted`；
- `Deleted` 表示逻辑删除，历史与 exact revision reference 仍保留；物理清除使用 `Purge`；
- `NotLoaded / Idle / Active / SystemError` 不是 durable Session lifecycle；
- load state、readiness、execution state 是进程内 transient projection；
- Session 不持有 `Arc<PromptService>`、`Arc<ToolService>`、`Arc<SkillService>` 或 ModelGateway；
- Runtime 在 loaded Session execution / Turn capture 时注入这些依赖；
- active Turn pin exact AgentRevisionRef 和 SessionDefinitionRevision；
- loaded Session的Workspace definition update只在Idle时接受；authority hard restriction通过SecurityRevoked中断active Turn；
- Session fork 复制 exact SessionDefinition 内容，但创建新的 SessionId 和独立 revision 序列；
- fork不复制loaded state、WorkspaceSnapshot、security signal、PromptSet、ToolSet、SkillView或provider session；
- process restart后所有Session都视为unloaded；不恢复旧ActiveTurnTask、stream、Tool task、approval waiter、Recorder object或in-flight append。

## 领域关系

```text
Agent
├─ current AgentRevision
└─ immutable AgentDefinition revisions

Session
├─ current SessionDefinitionRevision
├─ SessionDefinition
│  ├─ exact AgentRevisionRef
│  ├─ Workspace
│  ├─ SessionModelConfig
│  └─ SessionPromptSelection
└─ Conversation Storage JSONL semantic history

loaded Session execution
├─ exact SessionDefinition
├─ resolved Workspace state
├─ LiveSessionState seeded from recorded conversation prefix
├─ readiness
├─ execution state
└─ optional active TurnExecutionContext
```

Agent definition、Session definition、Session conversation 和 loaded execution state 是四类不同事实。

## Agent

Agent head 保存 identity、当前 definition pointer、durable status 和用户可见 metadata：

```rust
pub struct Agent {
    pub id: AgentId,
    pub current_revision: AgentRevision,
    pub metadata_revision: AgentMetadataRevision,
    pub status: AgentStatus,
    pub name: String,
    pub description: Option<String>,
    pub created_at: Timestamp,
    pub metadata_updated_at: Timestamp,
}
```

Agent execution definition 是 immutable revision value：

```rust
pub struct AgentDefinition {
    pub agent_id: AgentId,
    pub revision: AgentRevision,
    pub prompts: AgentPromptSelection,
    pub created_at: Timestamp,
}
```

未来 execution-affecting Agent fields，例如 default Tool/Skill policy、Model constraints 或 hooks，进入 AgentDefinition，而不是 Agent head。

`AgentDefinition` 不是独立 entity：

- 没有独立 lifecycle；
- 不能脱离 AgentId 创建；
- identity 是 `(AgentId, AgentRevision)`；
- 发布后不可原地修改；
- 由 Agent owner 保存和解析。

精确引用：

```rust
pub struct AgentRevisionRef {
    pub agent_id: AgentId,
    pub revision: AgentRevision,
}
```

## Agent Revision

`AgentRevision` 表示 Agent execution definition 的具体版本。

生成规则：

```text
创建 Agent
→ AgentRevision(1)

execution definition canonical content 改变
→ 创建新的更高 AgentRevision

name / description / status 改变
→ AgentRevision 不变
```

基础规则：

- revision 在单个 Agent 内单调递增；
- revision opaque，不要求 SemVer；
- revision 不复用，不原地覆盖；
- canonical candidate 与 current definition 等价时是 no-op；
- update 先 durable 写入完整 immutable definition，再原子移动 Agent current pointer；
- rollback 使用旧内容创建一个新的更高 revision，不把 current pointer 向后移动；
- 被 SessionDefinition、Turn metadata 或 fork provenance 引用的 revision 必须保留；
- revision GC 留到完整引用追踪可用后设计。

## Metadata Revision

```rust
pub struct AgentMetadataRevision(/* opaque */);
pub struct SessionMetadataRevision(/* opaque */);
```

两者是各自entity内独立、单调、不复用的CAS token：

```text
Create entity
→ metadata revision 1

canonical name/description change
→ metadata revision N+1

no-op metadata patch
→ revision不变

definition/status/lifecycle/load/conversation change
→ metadata revision不变
```

metadata revision不从Timestamp、definition revision或storage ordinal派生。CAS先验证expected metadata revision和non-deleted lifecycle/status，再做canonical no-op detection；stale expected token即使patch恰好等于current metadata也返回`StaleRevision`。definition与metadata revision不能互换。

## Agent Status

```rust
pub enum AgentStatus {
    Enabled,
    Disabled,
    Deleted,
}
```

状态机：

```text
Enabled ↔ Disabled
Enabled | Disabled → Deleted
Deleted → terminal
```

语义：

| 状态 | 可读 | 可更新 definition | 创建 Session | Session 升级 | future Turn | active Turn |
| --- | --- | --- | --- | --- | --- | --- |
| Enabled | 是 | 是 | 是 | 是 | 是 | 继续 pinned Context |
| Disabled | 是 | 是 | 否 | 否 | 否 | 继续 pinned Context |
| Deleted | 历史/audit 可读 | 否 | 否 | 否 | 否 | 继续 pinned Context |

`Deleted` 是逻辑删除：

- AgentId 永不复用；
- immutable definitions 不立即删除；
- existing Session history 仍可解析 exact AgentRevisionRef；
- 不级联 archive/delete Session；
- 普通 catalog/query 默认可以隐藏 Deleted Agent；
- physical `PurgeAgent` 是未来 retention/admin 操作。

Agent disable/delete 不承担 security revocation。已完成initiating UserMessage live admission的active Turn使用不可变Context，可以继续完成；需要立即停止时使用显式Turn cancellation或独立安全策略。

candidate admission在initiating UserMessage live apply前必须经过短Agent lifecycle synchronization。disable/delete与该live admission的胜负由该synchronization线性化，不能靠两个无保护状态读取猜测。

## Agent 操作

### Create Agent

```text
验证 initial AgentDefinition与metadata
→ lifecycle owner从CSPRNG取得sealed AgentId candidate
→ DurableState create_new permanent reservation（最多32次definite collision）
→ DurableState establishes complete generation-1 publication and installs immutable catalog
→ catalog { Enabled, definition revision 1, metadata revision 1 } becomes visible
```

create 必须complete-or-invisible。reservation成功后ID永不更换或复用；失败/crash的invisible staging可清理但reservation保留。DurableState alone establishes complete publication and installs the immutable catalog; physical paths、markers和generation不离开DurableState。

### Update Agent Definition

```text
expected current AgentRevision
+ candidate AgentDefinition
→ canonicalize / validate
→ no-op，或写入 revision N+1
→ 原子移动 current pointer
```

允许在Enabled或Disabled状态更新；Deleted返回terminal lifecycle error。definition update必须在Agent lifecycle synchronization内同时CAS expected AgentRevision和`status != Deleted`；metadata update在同一synchronization内CAS expected AgentMetadataRevision和`status != Deleted`。因此Delete线性化后不能发布迟到definition或metadata。

Agent update 不 fan-out 修改 Session，也不替换 active Turn Context。

Agent definition、metadata和status mutation只写Agent durable owner并更新Runtime read/observer surface；它们不写任一Session conversation JSONL，也不分配Session EntryId。metadata使用Runtime Interface冻结的独立event kind。

### Update Agent Metadata

```text
expected AgentMetadataRevision
+ canonical name/description patch
→ lifecycle synchronization内CAS status != Deleted
→ stale expected token: StaleRevision
→ canonical no-op: NoChange，revision不变，无event
→ durable metadata + updated_at + metadata revision N+1原子publication
```

metadata update不产生AgentRevision，也不改变active/future Turn execution definition。successful mutation返回new AgentMetadataRevision并发布独立`AgentMetadataUpdated` Runtime event；event detail携带mutation后的完整safe AgentSummary，因此host直接取得下一次metadata CAS token。

### Disable / Enable / Delete

- Disable、Enable和Delete使用Agent lifecycle synchronization；
- repeated Disable while Disabled / Enable while Enabled returns `NoChange`，不写generation、不递增revision且无event；
- Delete against already Deleted returns typed `AgentDeleted`，不写generation；Deleted 不允许 Enable；
- `CommandId`只在当前Runtime内作in-flight correlation；它不查询durable head、不跨restart恢复。Create/Fork response loss后host重新page/query catalog，blind retry可能产生新实体；
- loaded Sessions可以接收readiness invalidation，但initiating UserMessage live apply前的durable AgentStatus validation才是authoritative决策点。

## Session

Session head 保存 identity、当前 definition pointer、durable lifecycle 和用户可见 metadata：

```rust
pub struct Session {
    pub id: SessionId,
    pub current_revision: SessionDefinitionRevision,
    pub metadata_revision: SessionMetadataRevision,
    pub lifecycle: SessionLifecycle,
    pub fork_provenance: Option<SessionForkProvenance>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub created_at: Timestamp,
    pub metadata_updated_at: Timestamp,
}
```

Session不直接重复保存`agent_id`、Workspace、Model或SessionPromptSelection。这些execution definition fields属于`SessionDefinition`：

```rust
pub struct SessionDefinition {
    pub session_id: SessionId,
    pub revision: SessionDefinitionRevision,
    pub agent: AgentRevisionRef,
    pub workspace: Workspace,
    pub model: SessionModelConfig,
    pub prompts: SessionPromptSelection,
    pub created_at: Timestamp,
}
```

```rust
pub struct SessionModelConfig {
    pub selection: ModelSelection,
    pub reasoning: ReasoningPreference,
    pub max_output_tokens: Option<NonZeroU32>,
}
```

SessionDefinition只保存stable selection和用户偏好，不保存provider endpoint、credential、capabilities或client。Turn admission通过`ModelGateway.resolve_for_turn(...)`把它解析为exact `Arc<TurnModelSnapshot>`；catalog change只影响future Turn，active Turn不执行cross-model fallback。

基础关系：

```text
Session.current_revision
→ exact SessionDefinition
→ exact AgentRevisionRef
→ AgentDefinition
```

一个 Session 的所有 revisions 必须引用同一个 AgentId。切换到另一个 Agent 需要创建新 Session，不提供 `rebind_agent`。

Session 不持有：

```text
Arc<PromptService>
Arc<ToolService>
Arc<SkillService>
ModelGateway
WorkspaceSnapshot / process-local security signal
SkillView / LoadedSkill
ToolSet / PromptSet
conversation hot projection
active Turn / Interaction waiter
```

## Session Definition Revision

`SessionDefinitionRevision` 是 Session future Turn execution definition 的原子版本。

命名约定：`AgentRevision` 和 `SessionDefinitionRevision` 是 opaque、单调的 revision value；`AgentDefinition` 和 `SessionDefinition` 是由这些 revision value 标识的完整 immutable record。本文不冻结 revision 的整数表示。

以下变化创建新 revision：

```text
AgentRevisionRef
Workspace definition
Model
SessionPromptSelection
```

以下变化不创建新 revision：

```text
Session name / description
Session lifecycle
load / unload
conversation append / compaction
active Turn / pending Interaction
readiness / diagnostics
```

Workspace definition update 同时产生：

```text
新的 WorkspaceRevision
+ 新的 SessionDefinitionRevision
```

外层 revision 保证 Turn admission 不会读取到“新 Workspace + 旧 Model/Prompts”或其他跨 definition 组合。

更新规则：

```text
expected SessionDefinitionRevision
+ complete candidate SessionDefinition
→ validate
→ durable 写入 immutable revision N+1
→ 原子移动 Session current pointer
```

active Turn、Steer、retry和compaction继续使用old capture；非Workspace字段update线性化后admit的future Turn使用new revision。Workspace字段update对loaded Session另有Idle前置条件。

loaded Session只能缓存current definition head作为可丢弃projection。definition update成功commit后，execution owner在同一per-session lifecycle synchronization内应用新的committed pointer；Turn admission仍以durable current head/CAS为准，不能因为loaded cache过期而继续创建旧revision的future Turn。

Workspace definition与authority hard restriction分为两条路径：

```text
Workspace definition patch on loaded Session
→ require SessionExecutionState = Idle；否则SessionBusy
→ resolve candidate并捕获candidate授权的Workspace-bound Prompt/Skill sources
→ validate complete immutable WorkspaceSnapshot
→ durable commit 新 SessionDefinitionRevision
→ execution owner原子发布new WorkspaceSnapshot及captured source values
→ 返回 update success

authority/host hard restriction
→ authority或host先发布current policy/security fact
→ 通过current SessionExecutionHandle向affected loaded Session发布sticky SecurityRevoked
→ Idle直接失效old Snapshot；Starting取消candidate；active Turn停止新operation、truthful settle并TurnInterrupted
→ 使用 current exact SessionDefinition 重新resolve，并捕获新authority允许的Workspace-bound Prompt/Skill sources
→ 发布受限的新 Snapshot及captured source values，或 SessionReadiness = Unavailable
→ SessionDefinitionRevision 不变
```

Loaded Workspace update uses one SessionExecutor-owned `SessionDefinitionPublicationTask` with shared completion. The task—not the dispatch waiter—owns a distinct `SessionDefinitionPublicationPermit`, the prebuilt `WorkspaceSnapshot`, the DurableState actor request and final installation. It excludes Starting/admission from the successful Idle check through durable publication and installation; DurableStateActor does **not** reacquire this process-local permit. Caller/transport drop cannot cancel the owner task. Before `DurableCommitBarrier`, shutdown may reject and release; after it, the task must settle commit plus installation. If a post-commit install panic/invariant failure occurs, joined waiters receive existing outer `RuntimeDispatchError::InternalDispatchUnavailable` and Runtime integrity-closes rather than exposing a committed definition with no matching snapshot. Workspace candidate resolve或pre-barrier durable failure时，旧definition与旧Snapshot保持current，不存在“已撤权但未提交”的状态。SecurityRevoked不承诺撤销已经打开的OS handle或回滚已进入kernel/provider的operation；started Tool使用Cancel相同的truthful settlement规则。

## Session Pin Agent Revision

Session 创建时把 Agent 当时的 current revision解析为 exact `AgentRevisionRef` 并保存到 SessionDefinition。

Agent 发布新 revision 后：

```text
Agent current: A1 → A2
Session S definition 仍引用 A1
active Turn 继续使用 A1
future Turn 仍使用 A1
```

是否存在可升级版本可以派生：

```text
SessionDefinition.agent.revision != Agent.current_revision
```

不保存额外 `upgrade_available` flag。

显式升级：

```text
expected SessionLifecycle = Open
+ expected SessionDefinitionRevision
+ target: Option<AgentRevisionRef>   // None = 重钉 current（常规 reload 升级）
→ 按 Agent lifecycle → Session lifecycle 固定顺序获取 gates
→ 解析 target：None 时在 gates 内读取 Agent current 并固定为 exact ref
→ target 必须属于同一个 AgentId
→ Agent 必须 Enabled
→ target definition 必须存在
→ 在 gates 内创建新的 SessionDefinitionRevision
```

常规升级不报 revision：调用方发 `None`，Runtime 在 gates 内把 Agent current 解析成 exact AgentRevisionRef 后钉入。给出 exact `AgentRevisionRef` 用于钉指定/旧版或回滚。无论哪种，提交前都解析成 exact ref；`latest` 不进入 durable SessionDefinition。exact pin保证同一Session在两次升级之间的Agent selection、Workspace和Model配置稳定；显式Prompt resource reload仍可影响future Turn的PromptSet。

不引入：

```text
AgentBinding::FollowCurrent | Pinned
Agent channel / stable / preview
AgentUpgrade entity
```

## Session Lifecycle

```rust
pub enum SessionLifecycle {
    Open,
    Archived,
    Deleted,
}
```

状态机：

```text
Open ↔ Archived
Archived → Deleted
Deleted → terminal
```

语义：

| 状态 | history query/export | execution load | definition update | fork source | new Turn |
| --- | --- | --- | --- | --- | --- |
| Open | 是 | 是 | 是 | 是 | prerequisites 满足时是 |
| Archived | 是 | 否 | 否 | 是 | 否 |
| Deleted | audit/repair only | 否 | 否 | 否 | 否 |

`Open`表示durable Session允许definition update；若patch改变Workspace且Session已loaded，还必须是`SessionExecutionState::Idle`，否则返回`SessionBusy`。

`Archived` 是可逆只读状态：

- 保留 conversation、definitions、name 和 fork provenance；
- 普通 session list 可以默认隐藏或单独筛选；
- unarchive 后恢复为 Open；
- archive 不等于删除。

`Deleted` 是逻辑删除：

- 不可 unarchive；
- SessionId 不复用；
- baseline 保留 conversation 与 exact revision references；
- 普通 query 默认隐藏；
- physical `PurgeSession` 留给未来 retention/admin。

生命周期前置条件：

- archive 要求 Session 已 Unloaded；
- delete 要求 Session 已 Archived 且 Unloaded；
- archive/delete本身不隐式取消active Turn或卸载；调用方先执行Unload，Unload按有限grace deadline自然drain或fail-closed cancel，再进行archive/delete。

不使用裸 `close` 表示领域生命周期。协议应使用明确词：

```text
UnloadSession
ArchiveSession
UnarchiveSession
DeleteSession
PurgeSession
```

## Session Load State

load state 是 transient projection，不进入 durable Session：

```rust
pub enum SessionLoadState {
    Unloaded,
    Loading,
    Loaded,
    Unloading,
}
```

状态机：

```text
Unloaded → Loading → Loaded → Unloading → Unloaded
              └─ failure → Unloaded
```

实际实现可以用 private loaded-session index 的 presence 表达 Unloaded/Loaded，不要求持久化 enum 或新增 LoadState entity。

进程重启后所有 Session 默认是 Unloaded。

## Session Readiness

loaded 不代表当前可以执行：

```rust
pub enum SessionReadiness {
    Preparing,
    Ready,
    Unavailable(SessionUnavailable),
}

pub enum SessionUnavailable {
    AgentUnavailable,
    WorkspaceUnavailable,
    ModelUnavailable,
    PromptUnavailable,
    DurableStateCorrupt,
    DurableStateTooLarge,
    RuntimeDependencyUnavailable,
}
```

```text
Preparing → Ready
Preparing → Unavailable
Unavailable → Preparing   // retry/reload
```

示例：

```text
Session lifecycle = Open
SessionLoadState = Loaded
SessionReadiness = Unavailable(SessionUnavailable::WorkspaceUnavailable)
```

此时 history query 可以工作，但 Turn admission fail closed。

Unavailable 可以来自：

- exact AgentRevision 不可读或损坏；
- Agent Disabled/Deleted；
- Workspace root/authority unavailable；
- replay Header/required durable basis损坏；
- valid conversation file超过format-v1 1 GiB/1,000,000-entry hard cap；
- required Runtime dependency unavailable。

局部entry/history corruption本身只产生diagnostic并隔离projection；strict Header/required durable basis failure使用DurableStateCorrupt，hard size/count cap使用DurableStateTooLarge。`SystemError`不是 durable Session lifecycle。需要修复的问题通过 `Unavailable(reason)`、load error 或 recovery diagnostics 表达。

Readiness是projection，不是Agent/Session lifecycle admission的authoritative substitute。每次initiating UserMessage live apply前仍必须重新检查durable SessionLifecycle、AgentStatus、exact revisions和Workspace authorization。

## Session Execution State

execution state 只存在于 loaded Session：

```rust
pub enum SessionExecutionState {
    Idle,
    Starting,
    Running,
    Finishing,
}
```

状态机：

```text
Idle
→ Starting          // admission reservation / Context capture / Input live apply / record attempt / publication
→ Running           // TurnStarted已发布且ActiveTurnTask存在
→ Finishing         // Tool/process/filesystem结构化收口、live terminal与pending cleanup
→ Idle
```

失败路径：

```text
Starting failure → Idle
Running cancel/failure → Finishing → Idle
```

一个 loaded Session 同时最多一个 Starting 或 Running Turn。

这四个状态不进入durable Session。TurnStatus同样只属于loaded live execution；JSONL只用TurnId分组conversation facts。

## Turn Status 与 Execution Phase

Turn live status：

```rust
pub enum TurnStatus {
    Running,
    Completed { completed_at: Timestamp },
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

```text
Running → Completed
Running → Interrupted
Running → Failed
```

`Interrupted` 是 terminal status，只表示整个 Turn 已停止；不能继续恢复为 Running。

Turn 在 Running 期间使用 transient execution phase：

```rust
pub enum TurnExecutionPhase {
    Sampling,
    RetryBackoff,
    Compacting,
    WaitingApproval,
    WaitingForUserInput,
    ExecutingTools,
}
```

TurnStatus和这些phase都不进入JSONL。live mutation是各业务phase内的短线性化操作，不建立`Committing` phase；terminal settlement和Cancel后的结构化收口由`SessionExecutionState::Finishing`表达。`Compacting`仍保持`TurnStatus = Running`，并由ActiveTurnTask协调Cancel、Steer和SecurityRevoked。restart后`current_turn`为空。

`SessionExecutionState::Finishing`优先于TurnExecutionPhase解释：CancelAccepted后phase可以保留`ExecutingTools`等最后工作位置，但UI显示Stopping/Finishing。Finishing期间新的Steer被拒绝，FollowUp仍可进入process-local FIFO；新Turn只在旧Turnterminal并回到Idle后启动。

### Waiting Approval

等待审批时：

```text
TurnStatus = Running
SessionExecutionState = Running
TurnExecutionPhase = WaitingApproval
InteractionState = Pending
ToolInvocationState = Started
```

Approval：

```text
Allow → ExecutingTools
Deny  → 产生 denied ToolResult
→ append role=tool message
→ 同一assistant全部matching results存在时complete exchange
→ PreparingModel
```

等待审批不是 Interrupted。Interaction 和 parent ToolInvocation 的完整语义见 [Turn、Item 与 Interaction 架构设计](turn-item-interaction.md)。

### WaitingForUserInput

等待结构化UserQuestion回答时：

```text
TurnStatus = Running
SessionExecutionState = Running
TurnExecutionPhase = WaitingForUserInput
InteractionState = Pending
ToolInvocationState = Started
```

当前Turn的逻辑执行暂停在原ToolInvocation，但loaded Session及其Executor没有暂停：它继续处理`ResolveInteraction`、Cancel、SecurityRevoked、PrepareForUnload和Snapshot。等待期间不预留file mutation ticket，也不持有TurnControl reservation；其他Session拥有独立Executor，因此可以继续执行。用户长时间不回答时Interaction保持Pending。UserAnswer恢复同一Turn，不作为新UserMessage开启新Turn。

### Steer

Steer到达时Turn保持Running。请求必须携带`expected TurnId`，并在current-Turn validation中验证目标仍是同一个Running Turn：

```text
Steer(expected TurnId) accepted
→ push_back进入该Turn的SteerQueue
→ 当前assistant/tool step已在LiveConversation中完整
→ 下一次Model前pop_front一条并apply Steer live
→ 下一次模型调用看见 Steer
```

WaitingApproval或WaitingForUserInput中到达的Steer只进入该Turn的FIFO，不自动当作Interaction resolution。若live final arbitration先于Steer acceptance线性化，Steer不再属于该Turn；若Steer先进入FIFO，candidate final保存为Assistant Continue并在下一次Model前消费一条Steer。

只有显式Turn cancel、runtime shutdown、SecurityRevoked或不可恢复错误才使Turn terminal。

## Create Session

```text
验证host-neutral WorkspaceDefinitionInput / Model / SessionPromptSelection candidate
→ Workspace checked host-family lowering为durable WorkspaceRootSpec { path: PathBuf }
→ lifecycle owner从CSPRNG取得sealed SessionId candidate
→ DurableState create_new permanent reservation（最多32次definite collision）
→ actor gate之前只可准备不含assigned SessionId和AgentRevisionRef的parent-independent semantic/canonical candidate fragments
→ DurableStateActor获取Agent gate，读取current Enabled exact AgentRevisionRef
→ 在同一gate内构造final SessionDefinition(1)、SessionHeader和generation-1 head bytes
→ 写入/sync/exact-readback final markerless generation/entity payload
→ cross DurableCommitBarrier immediately before COMMITTED
→ 保持该gate直到complete PUBLISHED/readback后才install immutable catalog
```

create 的 baseline 结果是 `Open + Unloaded`，不创建SessionExecutor或SessionRecorder。Create的Agent gate跨final definition/Header/head construction、`DurableCommitBarrier`、`PUBLISHED`与complete readback，但绝不覆盖Recorder、SessionExecutor、event/fan-out或host callback。pre-gate fragment或已经`COMMITTED`的Header只要含stale Agent ref都不得被接受；publication末尾的“final checks”只是同一持续持有gate内的rechecks。SessionHeader/target staging失败或publication前crash只留下invisible removable staging，reservation仍烧毁；lifecycle caller不接收staging/path/generation/marker。create-and-load是上层组合，不改变create durable语义。

`WorkspaceDefinitionInput`保存`CanonicalFileUri` carrier而非native path。URI lexical-invalid时请求不能进入Runtime；canonical URI family不受current host支持时请求已经是typed command，由Workspace lowering返回`InvalidArgument + DoNotRetry`，且不得获取Agent synchronization、分配revision或开始Session/Header staging。

Create在host-neutral validation/lowering成功后由lifecycle owner取得CSPRNG SessionId candidate，并由DurableState永久reservation；成功reservation后sealed attempt绝不换ID。`CommandId`不进入该路径也不能查询其durable outcome。响应丢失或进程崩溃可能留下已发布、但host未知的Session；host必须重新page/query catalog，blind retry可能创建duplicate。这是V1有意不提供restart exactly-once Create的限制。

## Update Session Definition

只允许Open Session。普通definition update只能改变Workspace、Model或SessionPromptSelection；`AgentRevisionRef`变化必须走显式Agent upgrade路径，不能绕过Agent lifecycle synchronization。

若candidate改变Workspace且Session已loaded，额外要求`SessionExecutionState::Idle`；Starting/Running/Finishing返回`SessionBusy`，不排队、不隐式Cancel。successful loaded-Workspace path uses the SessionExecutor-owned `SessionDefinitionPublicationTask` described above: it owns the permit/shared completion across durable commit and infallible prebuilt `WorkspaceSnapshot` installation, independent of the dispatch waiter. Model或SessionPromptSelection的future-only update仍可在active Turn期间提交，因为它们不会改变current Turn captured Context。

```text
expected SessionLifecycle = Open
+ expected SessionDefinitionRevision
+ SessionDefinitionPatch（optional WorkspaceDefinitionInput仍为CanonicalFileUri roots）
→ 若含Workspace input，先checked host-family lowering为WorkspaceRootSpec { path: PathBuf }
→ lowering成功后形成complete candidate definition
→ for loaded Workspace, owner-register SessionDefinitionPublicationTask after successful Idle check
→ task acquires permit, awaits DurableState durable commit, then infallibly installs prebuilt WorkspaceSnapshot
→ settle shared completion and release permit only after installation
→ for other definition changes, DurableState establishes complete publication and installs immutable catalog for revision N+1
```

active Turn不受允许提交的future-only definition update影响。FollowUp在真正admission时读取update后的current revision。Workspace patch只有Idle时可提交，并在resolve与Workspace-bound Prompt/Skill source capture全部成功后发布new Snapshot。definition owner的CAS成功后更新loaded definition view；conversation Recorder health不参与该操作。

metadata update与definition update分开，避免改标题导致future Turn execution definition变化。metadata update可以作用于Open或Archived，并执行：

```text
expected SessionMetadataRevision
+ canonical name/description patch
→ per-session lifecycle synchronization内CAS lifecycle != Deleted
→ stale expected token: StaleRevision
→ canonical no-op: NoChange，revision不变，无event
→ durable metadata + updated_at + metadata revision N+1原子publication
```

successful mutation返回new SessionMetadataRevision并发布独立`SessionMetadataUpdated` Runtime event，detail携带mutation后的完整safe SessionSummary；Session已loaded时同一mutation也更新SessionSnapshot并发布Session-scope metadata event。definition/status/load/conversation mutation不递增该token。

Session definition/metadata update只写Session durable owner。loaded Session可以收到future-readiness/current-definition observer update或private invalidation，但该mutation不调用SessionRecorder、不生成StoredSessionEntry，也不与ActiveTurnTask竞争record order；metadata使用Runtime Interface冻结的独立event kind。完整conversation scope见[ADR 0131](../adr/0131-conversation-recording-excludes-session-definition-and-lifecycle.md)。

## Explicit Reload

共享Prompt/Skill/Tool/Model资源不属于durable Agent或Session definition。Runtime初始化后，它们只通过显式`/reload`替换current immutable objects；filesystem/config watcher最多标记dirty diagnostic，不自动publication。

`/reload`对共享资源执行two-phase流程：完整build candidates → validate所有required candidates → 短publication gate下原子替换四个current `Arc`。Turn admission在同一gate下只克隆四个current Arc；任一required candidate失败时保留全部old current values。active Turn继续使用已captured old Prompt/Skill/Tool/Model objects；reload成功后admit的future Turn使用new objects；completed Turn不更新。shared Prompt和Skill filesystem source在Runtime initialize/shared reload时读取为immutable captured bytes/content，reload失败继续使用old bytes。

`/reload workspace`是Session lifecycle操作，不是共享资源publication。loaded Session必须处于`SessionExecutionState::Idle`：Idle时重新resolve current SessionDefinition.workspace，并在candidate阶段捕获Workspace-bound Prompt/Skill sources；Ready时成功替换Snapshot、失败保留old Snapshot，Unavailable时成功恢复Ready、失败保持Unavailable。Starting/Running/Finishing返回`SessionBusy`，不排队、不隐式Cancel，也不原地替换active Turn Context。unloaded Session的Workspace在后续load时完成同一resolve/capture流程。

## Load Session

load 语义：

```text
在per-session residency/lifecycle synchronization内确认SessionLifecycle = Open
→ single-flight 标记 Loading，并 capture current SessionDefinitionRevision
→ 读取 Agent durable head / AgentStatus
→ 读取 exact AgentRevisionRef 对应的 AgentDefinition
→ replay recorded conversation prefix
→ DurableState提供root-lease-derived writable conversation proof时才允许tail truncate；ordinary recorder open/init失败建立Degraded health
→ 从replayed recorded head构建sanitized live state与new Recorder health
→ WorkspaceResolver::resolve(definition.workspace)得到candidate
→ PromptService/SkillService捕获candidate授权的Workspace-bound sources
→ candidate.finish得到immutable WorkspaceSnapshot
→ 注入 Runtime Prompt/Tool/Skill/Model dependencies
→ 重新进入synchronization，CAS lifecycle仍为Open且current revision未变化
→ 原子发布 loaded execution state
```

若 final CAS 发现 SessionDefinitionRevision 已变化，必须丢弃旧 resolve 结果并按新 revision 重试或返回 retryable stale error，不能把旧Workspace/SessionModelConfig/Prompt projection 发布为 current loaded state。

重复load幂等返回同一个loaded execution owner/handle，不重建Recorder，也不能借重复load把Degraded恢复为Healthy。只有先完成Unload、再执行新的Load，才创建新的loaded instance与Recorder health；该Load只恢复recorded prefix，旧unrecorded live tail永久丢失。

Agent Disabled/Deleted 时可以加载 history projection，但 readiness 为 Unavailable，新 Turn admission 被拒绝。

exact AgentRevision缺失或定义损坏时不能替换为Agent current revision。recorded history若仍可读取可以返回diagnostics；execution fail closed。

load 不恢复：

```text
provider stream
ActiveTurnTask
Tool task
approval waiter
cancellation token
old SessionRecorder object / in-flight append / unrecorded live tail
old WorkspaceSnapshot / EmergencyControl signal
PromptSet / ToolSet / SkillView
```

## Unload Session

unload不修改durable Session lifecycle或SessionDefinitionRevision。若grace deadline后必须停止active work，ActiveTurnTask仍按普通规则完成live Interaction closure、truthful Tool settlement、TurnInterrupted publication和必要conversation record attempts；recording失败不阻止unload。

```text
Loaded
→ LifecycleControl::PrepareForUnload(grace_deadline)
→ stop new Submit/Steer/FollowUp admission
→ reject queued Submit admissions并从QueueView移除其CommandId
→ clear queued Steer/FollowUp并完成其typed outcome
→ grace期内允许active admission/Turn自然完成，继续处理Interaction resolution和truthful Tool outcome
→ deadline到期仍未Idle：Cancel active pre-Turn Submit或Turn，并以Cancelled关闭Pending Interaction
→ ActiveTurnTask terminal settlement完成
→ Unloading
→ drop resolved Workspace state
→ drop LiveSessionState
→ drop execution owner/runtime handles
→ Unloaded
```

规则：

- grace deadline必须有限，由Runtime config定义上限；它属于显式Unload lifecycle，不是Interaction inactivity timeout；
- grace期内显式Cancel可以加速结束，但调用方不需要先手工cancel/resolve再调用Unload；
- Cancel current Turn默认保留FollowUp，但PrepareForUnload已经在stop-admission时清理queued FollowUp，因此不会在卸载前启动新Turn；
- 已取得ToolStartPermit并进入Running/Settling的Tool必须先确认truthful outcome或记录Abandoned，不能为了卸载直接drop；
- ActiveTurnTask结束后关闭ingress；Recorder没有后台queue或drain step，Runtime随后从loaded map移除handle；Degraded health与unrecorded live tail随loaded instance一起销毁；
- 重复 unload 订阅同一个LifecycleControl completion waiter并幂等返回同一个最终结果；effective deadline只能缩短，不能被后续请求延长。

Runtime shutdown 使用：

```text
逐Session发布PrepareForUnload
→ 各Session独立等待graceful Turn settlement
→ deadline到期后各自fail-closed cancel
→ wait ActiveTurnTask settlement
→ unload
```

## Archive / Unarchive / Delete

### Archive

```text
Open + Unloaded
→ Archived
```

archive 不隐式 unload，也不取消 Turn。Archive只更新Session durable lifecycle并发布current Runtime StateEvent；它不需要Recorder或LiveSessionState EntryIdGenerator，也不写conversation entry。对已经Archived的Session重复Archive返回`NoChange`，不写generation且无第二event。

### Unarchive

```text
Archived
→ Open + Unloaded
```

Workspace 和 exact Agent revision 在后续 load/admission 时重新验证。Unarchive只更新Session durable lifecycle，不写conversation entry。对已经Open的Session重复Unarchive返回`NoChange`，不写generation且无event。

### Delete

```text
Archived + Unloaded
→ Deleted
```

delete 不级联删除 Agent、Workspace files、fork children 或 sibling Sessions。Delete只更新logical durable lifecycle；physical conversation file retention/purge是独立operations问题，不追加lifecycle terminal到JSONL。Delete against already Deleted returns typed `SessionDeleted` and writes nothing；Open→Deleted仍是invalid lifecycle transition。

## Fork Session

Session fork 与同一 Session 内 branch 是不同概念：

```text
Branch
→ 同一 SessionId 内改变 conversation leaf

ForkSession
→ 创建新的 SessionId

CloneAgent
→ 创建新的 AgentId
```

本文只定义 ForkSession lifecycle 语义。

前置条件：

- source 为 Open 或 Archived；
- source 不为 Deleted；
- public boundary是genesis，或ConversationStorage定义的UserMessage/final AssistantMessage前后anchor；Compaction本身不作为MVP公开anchor，位于selected message path上的Compaction自然随path生效；
- selected path可以包含没有Final AgentMessage的conversation tail；target原样复制该path，child不继承source current Turn；
- stream draft不属于recorded prefix；若selected prefix含Pending Interaction、Started ToolInvocation或incomplete Tool exchange，不恢复旧waiter/Tool task，也不合成ToolResult；history仍可读，model conversation隔离不完整exchange；
- source SessionDefinition 和 exact AgentRevision 仍可读取；
- Agent 必须 Enabled，才能发布 Open child Session。

fork capture：

```text
source SessionDefinition at fork linearization point
+ loaded时同一LiveSnapshot中的anchor与selected path
+ unloaded时tolerant replay得到的RecordedHistory path
+ durable SessionForkProvenance
```

source kind由同一residency/lifecycle synchronization决定。source在该linearization point已loaded时必须使用`LiveSnapshot`，即使Recorder已经Degraded或当前entry的record attempt尚未返回；source未loaded时由DurableState/source-residency owner签发typed immutable `RecordedForkConversationLease`，绑定exact source Session/file observation并令source Load、append与tail truncate等待整个streamed copy。Conversation Storage消费该lease来replay、resolve anchor并形成semantic seed。Unload先赢则走RecordedHistory，Fork先完成capture/lease则后续Unload不改变该Fork。不得从loaded source静默fallback到RecordedHistory，因为这会丢失用户当前可见的unrecorded tail。

LiveSnapshot在短live-state critical section内同时解析anchor并复制immutable selected path；RecordedHistory先在短residency/lifecycle guard内取得typed physical lease。短guard都在target I/O前释放；recorded physical lease继续覆盖整个copy/readback，完成后释放，且绝不与最终Agent permit重叠。capture前已apply的事实进入child，capture后apply的事实不进入；stream draft和process-local queue不进入。copy runs as ConversationStorage semantic streaming re-encode in an actor-owned/tracked job up to the 1 GiB cap: it emits a child Header and rebinds only every selected entry's SessionId, never raw-copies source JSONL bytes. target selected path未完整materialize并通过replay validation时不发布child。

child：

```text
new permanently reserved SessionId
SessionDefinitionRevision(1) / WorkspaceRevision(1)
exact copy of captured source AgentRevisionRef（final publication仍验证definition存在且Agent Enabled）
copy Workspace semantic fields、source Model / SessionPromptSelection
name = None, description = None, SessionMetadataRevision(1), child-local timestamps
Open + Unloaded with durable Fork provenance
independent future revisions / WorkspaceSnapshot / process-local security signal / conversation branch
```

不复制：

```text
source SessionDefinitionRevision / WorkspaceRevision number
loaded execution state
WorkspaceSnapshot / process-local security signal
ToolSet / PromptSet / SkillView
provider session
pending Interaction / 任何process-local SessionIngress lane
```

fork由DurableState operation-owned immutable generation/publication完成。Conversation Storage在final markerless child path完成streaming re-encode、sync、full bounded-memory reread并产生`PreparedConversationProof`；不创建fork-specific terminal，也不合成ToolResult。selected path完整materialize后，actor才取得Agent gate，检查captured exact Agent definition仍存在且AgentStatus = Enabled，cross `DurableCommitBarrier`，并保持该gate直到`COMMITTED`/`PUBLISHED`/complete readback。它与Agent disable/delete线性化；失败或crash的invisible target不进入Session catalog、reservation不回收。provenance source ID必须等于实际captured lease/seed source且不能等于child。child发布为`Open + Unloaded`，future Load从copied transcript建立Idle view。

child definition由本lifecycle owner从source durable SessionDefinition构造revision 1；conversation copy只包含selected User/Assistant/Tool、Interaction和Compaction facts，不复制source definition/metadata/lifecycle transition timeline。

Conversation Storage使用`EntryId + parent_id` entry tree；Fork semantic streaming re-encode preserves selected historical EntryId、parentId、TurnId、ItemId、RequestId、ToolCallId、timestamp、body和order，writes a new child Header, and rebinds only `sessionId` to the child; raw source JSONL byte-copy is prohibited. target materialize完成后用全部copied EntryId初始化child `LiveSessionState` collision guard；future EntryId由child私有Session-scoped generator分配，不执行nested remap。exact typed ID carrier由[ADR 0134](../adr/0134-public-and-conversation-wire-use-bounded-v1-schemas.md)冻结；完整storage规则见[Conversation Recording与Replay](conversation-storage.md)，公开Genesis/UserMessage/FinalAgentMessage anchor payload见[Runtime Interface](runtime-interface.md)。

## Turn Admission Basis

Turn admission 必须从一个 exact Session definition 捕获：

```text
SessionId
SessionDefinitionRevision
SessionDefinition.agent = AgentRevisionRef
SessionDefinition.workspace
SessionDefinition.model
SessionDefinition.prompts
```

推荐顺序：

```text
Session lifecycle/load/readiness/execution validation
→ reserve candidate Turn
→ capture exact SessionDefinitionRevision
→ 读取 exact AgentDefinition 并 capture WorkspaceSnapshot
→ capture SkillView / ToolSet / PromptSet
→ 获取短Agent lifecycle synchronization
→ 最终检查 AgentStatus = Enabled
→ 在gate内apply Input UserMessage + Turn Running
→ release `AgentAdmissionPermit` before any DurableState actor request or Recorder await
→ complete current Input record attempt and publish TurnStarted
```

Input entry只保存conversation内容、TurnId相关性和safe part-level contribution stamps，不内联Turn-start execution snapshot或source authorization。actual response model由后续`StoredAssistantMessage.model`说明；AgentRevisionRef、SessionDefinitionRevision和Workspace配置由各自durable owner保存，restart不据此重建旧execution environment。

Agent current revision不参与Turn capture，因为Session已经pin exact AgentRevisionRef。Agent status的authoritative check与initiating UserMessage live apply使用同一个短lifecycle synchronization；capture前的status/readiness check只用于提前失败。Recorder await发生在释放该synchronization之后。

## Active 与 Future Turn

active Turn资源固定的canonical合同见[INV-201](../architecture.md#跨模块不变量索引)；本节只描述Agent/Session lifecycle mutation对candidate、active和future Turn的影响。

| 操作 | candidate admission | active Turn | future Turn |
| --- | --- | --- | --- |
| Agent 发布新 revision | 保持 exact Session pin | 不变 | 仍使用 Session pin，直到显式升级 |
| Session 显式升级 Agent | 已捕获旧 SessionDefinitionRevision 者不变 | 不变 | 使用新 AgentRevisionRef |
| Session definition non-Workspace update | 已捕获旧revision者不变 | 不变 | 使用新SessionDefinitionRevision |
| Agent disable/delete | Input live apply前的synchronization决定胜负 | 已开始Turn继续 | admission拒绝 |
| Session archive/delete | 要求先无 candidate | 要求先无 active Turn | lifecycle 状态拒绝 |
| Workspace definition update | candidate存在/非Idle时SessionBusy | active时SessionBusy，不提交update | Idle commit后使用new definition/snapshot |
| authority/host hard restriction | Input apply前取消candidate；apply后绑定同一Starting Turn并阻止task spawn | SecurityRevoked + truthful settlement + TurnInterrupted | Idle直接resolve；active terminal后resolve，Ready或Unavailable |
| unload | stop admission并等待或取消candidate | grace期自然完成，deadline后fail-closed cancel | unload后需重新load才可admission |

## 并发线性化

### Definition Update vs Turn Admission/Load

Session admission synchronization保证candidate得到完整旧SessionDefinition或完整新SessionDefinition，不允许字段跨revision混合。Loading使用captured revision构建临时状态，并在publication前CAS current head；definition update先赢时旧load result被丢弃。

Workspace patch还与execution admission在同一per-session lifecycle synchronization中检查Idle：update先赢则new Snapshot publication完成后才能admit；Submit/FollowUp先把state推进Starting则Workspace patch返回`SessionBusy`。不提供active update等待或排队。

### Agent Disable/Delete vs Turn Start Append

短Agent lifecycle synchronization决定：

```text
status mutation先赢
→ candidate Input live apply被拒绝

initiating UserMessage live apply先赢
→ Turn使用pinned Context继续；后续record attempt不改变胜负
```

Turn admission may hold an `AgentAdmissionPermit` from final Enabled check through initiating UserMessage live apply, but performs no DurableState actor request while holding it and releases it before the Recorder await; active Turn never retains it. That permit and DurableStateActor's exclusive status-mutation gate are two sides of the same private per-Agent `AgentLifecycleGate`/epoch, so Disable/Delete and Input apply have one explicit linearization point. No caller holds an Agent or durable Session lifecycle/mutation gate while awaiting DurableStateActor. CreateSession, Session Agent upgrade, and Fork child final publication acquire required private durable gates inside the actor in `Agent → Session` order. For CreateSession, the actor reads the current Enabled exact ref, builds and markerless-writes/readbacks the final definition/Header/generation-1 head under one Agent gate, crosses `DurableCommitBarrier` immediately before COMMITTED, then holds through `PUBLISHED`/complete readback; any final checks are rechecks under that same gate. Fork completes its large markerless copy first, then acquires/holds the Agent gate from exact final Enabled check through markers/readback. The actor may hold the Agent gate through bounded-size but potentially unbounded-latency local publication I/O (an explicit head-of-line tradeoff), but never across Recorder, SessionExecutor, event/fan-out, or host callback.

### Session Definition/Metadata Update vs Archive/Delete

Session definition update必须在per-session lifecycle synchronization内同时CAS `SessionLifecycle::Open`与expected SessionDefinitionRevision；metadata update CAS `lifecycle != Deleted`与expected SessionMetadataRevision。Archive/Delete先赢时不满足该operation前置条件的迟到update失败；update先赢时lifecycle mutation观察新的durable head。

跨Agent/Session操作使用固定synchronization顺序`Agent lifecycle → Session lifecycle`，避免Agent disable、Session upgrade/fork和archive之间形成锁环。

该顺序只适用于private durable state synchronization inside DurableStateActor. Callers must not await the actor while holding an Agent or durable Session lifecycle/mutation gate. The actor does not take SessionExecutor's process-local `SessionDefinitionPublicationPermit`; the owner-registered `SessionDefinitionPublicationTask` retains that permit, prebuilt Snapshot and shared completion across Idle check、durable commit与infallible install，dispatch waiter drop不取消它。实现不得在持有Agent lifecycle guard/permit时等待SessionRecorder、SessionExecutor、Unload completion、event subscriber或host callback；status/revision mutation完成后先释放gate，再向loaded Sessions fan-out readiness invalidation。普通Mutex/RwLock guard不得跨`.await`；initiating Input的final Enabled check与live apply由不跨I/O的typed admission permit和私有组合操作收口（ADR 0137）。

### Load vs Load

同一 Session load single-flight；不能创建两个 execution owners。

### Load/Admission vs Unload/Archive/Delete

使用同一个per-session residency/lifecycle synchronization串行化。不能让旧loaded state和新loaded state同时存在。

### Entry Append vs Cancel/Unload

inline `SessionRecorder.record().await`一旦进入当前physical write attempt，不由run cancellation截断。sticky Cancel/SecurityRevoked仍可立即发布；同Session terminal publication和Unload completion等待当前recordable mutation attempt返回。Terminal本身不追加JSONL entry。

## Crash Recovery

ConversationStorage的物理扫描与Tool exchange sanitizer以[INV-002和INV-003](../architecture.md#跨模块不变量索引)为准；本节只拥有Session load/readiness。JSONL不保存Turn lifecycle，因此Load没有unfinished Turn closure步骤。

Runtime restart：

```text
durable Agent/Session definitions保留
conversation只保留SessionRecorder已写入的prefix
所有 loaded Session execution state 消失
→ SessionLoadState = Unloaded
```

显式 load 时：

1. 读取Session durable definition owner并校验lifecycle；
2. 读取current exact SessionDefinitionRevision与exact AgentRevisionRef，供future Turn使用；
3. 调用ConversationStorage执行INV-002 tolerant replay并取得recorded selected path、sanitized live seed与diagnostics；
4. 保留INV-003认定的complete Tool exchange，隔离incomplete/orphan exchange；
5. 在replayed recorded head初始化新SessionRecorder，结果为Healthy或Degraded；
6. 重新resolve Workspace；
7. 不恢复旧Tool task或Interaction waiter；complete Tool exchange保留，incomplete exchange从model conversation排除，unmatched Interaction request不进入active pending view；
8. 设置`current_turn = None`，不推断旧Turn outcome且不执行recovery append；
9. 进入Idle Ready、WorkspaceUnavailable或带recording warning的状态。

MVP不保存或加载ProjectionSnapshot/checkpoint index；完整recorded-prefix replay的线性成本是有意取舍。Runtime内切换不同已loaded Session只路由现有SessionExecutionHandle。恢复不加载旧provider stream、ActiveTurnTask、Tool task、waiter、queue或其他process-local执行状态。

禁止：

- 用Agent current revision替代Session pin来构造future Turn；
- 从recorded TurnId、UserMessage tail或缺少Final AgentMessage推断旧TurnStatus；
- 恢复旧provider stream、ActiveTurnTask、Tool task或approval waiter；
- 自动重放outcome unknown的非幂等Tool；
- 用last-good WorkspaceSnapshot绕过当前authority；
- 自动reparent、合成ToolResult或重写中段损坏记录。

## 错误分类

只固定语义分类，不冻结公开 error enum：

```text
NotFound
AgentDisabled
AgentDeleted
SessionArchived
SessionDeleted
StaleAgentRevision
StaleSessionDefinitionRevision
AgentMismatch
SessionBusy
SessionUnavailable
RevisionUnavailable
InvalidLifecycleTransition
RecordedStateCorrupt
IdConflict
StoreInUse
DurableStateCorrupt
DurableStateTooLarge
RuntimeClosing
```

重试建议：

- stale/busy：重新读取对应definition owner或recorded prefix后决定；durable publication certainty indeterminate会poison/close，不作为可按CommandId查询或普通retry的`OutcomeUnknown`；
- disabled/archived：需要显式 enable/unarchive；
- deleted/invalid transition：普通流程不可恢复；
- unavailable/current definition revision missing：修复Workspace/dependency或definition retention后retry admission/load；
- history corruption：保留可解释recorded history与replay diagnostics；普通load不自动repair，中段坏记录被skip/isolate。

## 方案比较

### Session 跟随 Agent Current

优点：类型和操作少；Agent 更新自动影响所有 Session future Turn。

缺点：同一 Session 行为无感漂移；fork 首个 Turn 可能与 source 不同；更新 blast radius 大；recovery 仍必须额外保存历史 exact revision。

### Session Pin Exact Agent Revision

优点：SessionDefinition 自包含；future Turn、fork、审计和 recovery 可解释；Agent update 不产生隐式 fan-out。

缺点：需要显式升级；旧 Session 可能长期停留旧 revision；revision retention 成本更高。

### Agent Stable/Preview Channel

优点：支持 staged rollout、promotion 和 rollback。

缺点：增加 channel state、promotion CAS、fork 漂移、GC roots 和 UI 状态。当前没有足够产品需求。

### 决策

MiniCore 使用 exact pin：

```text
SessionDefinition.agent = AgentRevisionRef
```

不同时支持 follow-current/pinned 两种模式，也不引入 Agent channel。未来若出现真实 release workflow，channel 只能作为创建或显式升级 Session 时解析 exact revision 的 convenience alias，不能成为 Session 的持续动态绑定。

## 与Runtime-Owned共享模块的关系

PromptService、ToolService、SkillService和ModelGateway都由MiniCoreRuntime创建并在Runtime生命周期内共享。前三者提供Turn capture所需的资源/执行view，ModelGateway提供exact model resolution和单次provider attempt。

它们不进入 durable Agent 或 Session：

```text
MiniCoreRuntime internal services
→ injected into loaded Session execution
→ used to capture TurnExecutionContext
```

AgentDefinition 和 SessionDefinition 只保存各自拥有的 durable configuration，不保存 service handle、cache、Catalog 或 executor。

## 明确不建立的对象

```text
AgentManager
AgentLifecycleService
AgentRevisionService
SessionManager
SessionLifecycleService
SessionDefinitionManager
AgentUpgrade entity
Fork entity / ForkManager
Archive entity
Delete/Tombstone entity
LoadRecord entity
RecoverySession entity
Agent load/unload state
AgentBinding mode
Agent release channel
```

必要的 private loaded-session index 只是 Runtime residency 路由，不提升为领域 entity 或公开 registry。

## 基础不变量

- Agent是durable reusable definition owner，不是running ActiveTurnTask；
- Agent revision immutable，old revision 不被 current update 覆盖；
- Agent status 与 AgentRevision 正交；
- Session pin exact AgentRevisionRef；
- Agent current update 不自动改变 Session；
- Session 只能升级同一个 AgentId 的 revision；
- SessionDefinitionRevision原子绑定AgentRevisionRef、Workspace、SessionModelConfig和SessionPromptSelection；
- Session 不持有 Runtime Service handle；
- Session durable lifecycle 与 load/readiness/execution state 分离；
- Agent/Session definition、metadata与durable lifecycle不写conversation JSONL；
- Agent/Session metadata revision从1开始，只随canonical metadata mutation递增；
- stale metadata CAS即使patch等于current也失败，no-op只在CAS成功后返回且不发布event；
- Open 不等于 Loaded；Loaded 不等于 Ready；Ready 不等于 Running；
- Deleted 是逻辑删除，Purge 才是物理清除；
- archive/unload/delete 不使用同一个 close 语义；
- active Turn 使用 captured exact definitions，不受 ordinary update 影响；
- Agent disabled/deleted与CreateSession、upgrade、fork publication和initiating UserMessage live apply使用同一lifecycle synchronization线性化；
- Create SessionHeader staging失败或publication前crash不发布Session catalog entry；
- Agent disabled/deleted 阻止 future admission，但不 patch active Context；
- Workspace definition update只在Idle；authority/host hard restriction通过SecurityRevoked中断active Turn；
- WaitingApproval和WaitingForUserInput时Turn仍是Running；
- Steer 不把 Turn 变成 Interrupted；
- fork从genesis或公开message anchor创建；selected conversation path原样复制，child不继承source current Turn；
- fork 不复制 loaded execution state或 authorization capability；
- host restart后loaded state全部丢失，只能由durable definitions与recorded conversation prefix重建；
- recovery 不使用 current revision 冒充历史 exact reference；
- recovery不重建旧TurnStatus或active Pending Interaction；incomplete Tool exchange从model conversation排除，Load不执行closure append。

## Test Matrix

至少覆盖：

- DurableState permanent reservation、32 definite collision cap、reservation-after-crash/no-ID-reuse与no durable CommandId correlation；
- root lease StoreInUse/reacquire/invalid identity、strict case/link/reparse namespace rejection、cap+1 scanner、exact staging cleanup failure blocks open；
- Agent/Session create/update/fork generation CAS/no-op、DurableState complete-publication boundary、Completed-before-Closing、committed-corrupt与indeterminate poison；
- child metadata为None/metadata revision 1、source LiveSnapshot/RecordedHistory lease blocks append/load/tail truncate、source later mutation不改变captured child；
- Create/Fork response loss：host re-page/query catalog，blind retry may duplicate，不宣称restart exactly-once；
- create Agent 发布 revision 1；
- Agent definition no-op update 不递增；
- concurrent Agent update stale revision；
- Agent rollback 创建更高 revision；
- Enabled/Disabled/Deleted transition；
- Disabled Agent 允许读和 update definition，但拒绝 Session create/upgrade/admission；
- Deleted Agent history revision 可读但 future operation拒绝；
- create Session pin Agent current revision；
- Agent 更新不改变已有 Session；
- explicit Session Agent upgrade；
- upgrade AgentId mismatch；
- SessionDefinition update CAS；
- Agent/Session metadata independent CAS、readback、no-op与delete/archive竞态；
- loaded/unloaded definition与metadata update均不append conversation entry；
- Workspace update 同时改变 WorkspaceRevision 和 SessionDefinitionRevision；
- Open/Archived/Deleted transition；repeated Enable/Disable/Archive/Unarchive exact `NoChange`，repeated Delete exact typed Deleted且都不写generation/event；
- Archive/Unarchive/Delete不需要Recorder或EntryIdGenerator，且不append lifecycle event；
- archive loaded Session 返回 conflict；
- delete 非 Archived/loaded Session 返回 conflict；
- load single-flight，且 definition update 先赢时旧 load publication CAS 失败；
- load exact Agent revision，不用 current 替代；
- Degraded状态下重复Load幂等返回同一owner，不能恢复Recorder；
- Unload/Load只恢复recorded prefix并重新建立Recorder health；
- writable Load只截断final unterminated partial tail，保留完整newline-terminated entry与中段内容；
- Workspace unavailable 形成 Loaded + Unavailable；
- unload stop-admission、grace deadline、fail-closed cancel与幂等completion waiter；
- Session execution Idle/Starting/Running/Finishing；
- Cancel sticky epoch发布后立即返回CancelAccepted并进入Finishing；
- Finishing期间FollowUp可Queued，旧Turnterminal前不启动新Turn；
- WaitingApproval保持Turn Running，长时间无回答不产生默认Deny；
- subscriber断开后Pending Interaction保持并可由新Snapshot重建；
- Steer 在 WaitingApproval 时排队；
- WaitingApproval Steer只进入FIFO，不作为approval decision；
- WaitingForUserInput保持Turn/Session execution Running，且不预留file mutation ticket；
- WaitingForUserInput时Steer只排队，UserAnswer恢复同一Turn；
- 一个Session等待UserQuestion时，其他Session继续运行；
- Agent disable/delete vs initiating UserMessage live apply final synchronization；
- CreateSession/Agent upgrade/Fork publication vs Agent disable/delete；
- Session definition/metadata update vs archive/delete lifecycle synchronization；
- Session definition update vs admission 捕获完整旧/新 revision；
- entry append vs cancel/unload；
- fork Open/Archived source，覆盖terminal boundary与mid-Turn message anchor；
- loaded source使用同一LiveSnapshot解析anchor并复制unrecorded tail，unloaded source使用RecordedHistory；
- Fork与Unload竞态按source residency linearization选择并保存ForkSourceKind；
- live Fork staging失败不发布partial child；
- fork 复制 exact AgentRevisionRef 与 definition content，但创建 child-local WorkspaceRevision(1)；
- fork不复制Snapshot/security signal/ToolSet/PromptSet/SkillView；
- fork copied EntryId seed child collision guard，future EntryId由child live owner分配且不碰撞；
- fork staging crash不发布target；conversation tail fork不恢复source执行状态或追加terminal；
- fork只复制conversation path，child definition来自durable lifecycle staging且不含source lifecycle timeline；
- loaded Session非Idle时Workspace definition update返回SessionBusy且不排队；
- Idle Workspace update成功同时改变WorkspaceRevision和SessionDefinitionRevision并发布new Snapshot；
- Workspace candidate resolve/commit失败保留old definition/Snapshot；
- authority/host SecurityRevoked不创建SessionDefinitionRevision，Turn terminal后重新resolve；
- started Tool在SecurityRevoked下truthful settlement，open handle不承诺动态撤销；
- Starting SecurityRevoked在Input live apply前不创建Turn，apply后绑定同一Turn并阻止ActiveTurnTask spawn；
- Steer expected TurnId与live final arbitration race；
- restart 后所有 Session Unloaded；
- restart后current_turn为空，不推断或追加旧Turn terminal；
- recovery不恢复pending Interaction waiter或active ToolInvocation；已有conversation facts保留，incomplete Tool exchange从model conversation隔离；
- unmatched Interaction或Started ToolInvocation产生replay diagnostic，history仍可读，future admission只由definition/Workspace readiness决定；
- current exact revision缺失时future Turn admission fail closed；recorded conversation read不依赖historical Turn-start execution ref；
- Deleted identity 不复用；
- Purge 不属于普通 lifecycle。

## 后续问题

1. AgentDefinition 未来是否持有 Tool/Skill/Model constraints。
2. Session list 如何投影 load/readiness/execution state。
3. auto-unload policy、idle timeout 和 subscription 对 residency 的影响。
4. future physical purge、retention、revision reachability GC和durable idempotency-key design（均不属于V1）。
5. auto-unload与per-session subscription residency policy的具体默认值。

## 设计进度

- [x] 区分 Agent entity 与 immutable AgentDefinition revision。
- [x] 固定 AgentRevision 生成、保留和 rollback 规则。
- [x] 固定 Agent Enabled/Disabled/Deleted lifecycle。
- [x] 选择 Session pin exact AgentRevisionRef。
- [x] 定义显式 Session Agent upgrade。
- [x] 定义 SessionDefinitionRevision 和原子字段集合。
- [x] 区分 Session durable lifecycle 与 load/readiness/execution state。
- [x] 定义 Session Open/Archived/Deleted lifecycle。
- [x] 定义 create/update/load/unload/archive/unarchive/delete。
- [x] 定义 fork 的 definition copy 和 stable boundary 语义。
- [x] 定义 active/future Turn 和 lifecycle race。
- [x] 定义 WaitingApproval、WaitingForUserInput、Steer 和 Interrupted 的关系。
- [x] 定义 conservative crash recovery。
- [x] 完成operation-centric Item、live Interaction和terminal cleanup类型。
- [x] 完成Session ledger identity、entry parent tree、Fork保留历史ID、strict append与tolerant replay contract。
- [x] 明确Agent/Session definition、metadata和lifecycle不进入conversation JSONL（ADR 0131）。
- [x] 完成SessionExecutor owner和crate-private request interface。
- [x] 完成公开Runtime interface设计，见[Runtime Interface](runtime-interface.md)。
- [x] M5.0冻结DurableState、Durable Store V1、root lease、permanent reservations（new-entity Create/Fork complete-or-invisible、existing-head update old-or-new）与Tokio/deterministic test seams；production recovery/root lease/owner-tracked actor、private permanent reservation foundation、crate-private Agent Create与ordinary Session Create exact G1 COMMITTED/PUBLISHED publication、unloaded RecordedHistory + Genesis Session Fork tracer、Agent status/definition/metadata、Session metadata CAS、Session definition/Agent revision upgrade CAS、Session lifecycle existing-head action tracer，以及exact historical Agent/Session definition resolution已实现；public Runtime command接入、remaining Fork anchors/LiveSnapshot、loaded Workspace Idle/Snapshot publication、Session lifecycle Runtime residency integration、完整cross-platform native matrix、Recorder/replay仍pending。
