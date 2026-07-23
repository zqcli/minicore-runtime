# Agent 与 Session 生命周期架构设计

状态：目标架构已确定；公开protocol和实现待后续阶段完成
日期：2026-07-16

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

- Agent/Session durable store的具体实现类型；
- Runtime command、query、event 和 transport payload；
- SessionStorage entry/fork remap 的重复定义；这些以 [Conversation 与 SessionStorage 架构设计](conversation-storage.md) 为权威；
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
- SessionDefinition原子绑定AgentRevisionRef、Workspace、SessionModelConfig和SessionPrompts；
- Session durable lifecycle 使用 `Open / Archived / Deleted`；
- `Deleted` 表示逻辑删除，历史与 exact revision reference 仍保留；物理清除使用 `Purge`；
- `NotLoaded / Idle / Active / SystemError` 不再是 durable Session lifecycle；
- load state、readiness、execution state 是进程内 transient projection；
- Session 不持有 `Arc<PromptService>`、`Arc<ToolService>`、`Arc<SkillService>` 或 ModelGateway；
- Runtime 在 loaded Session execution / Turn capture 时注入这些依赖；
- active Turn pin exact AgentRevisionRef 和 SessionDefinitionRevision；
- definition update 只影响 future Turn，Workspace security restriction 仍可 revoke active Turn；
- Session fork 复制 exact SessionDefinition 内容，但创建新的 SessionId 和独立 revision 序列；
- fork 不复制 loaded state、WorkspaceSnapshot、lease、PromptSet、ToolSet、SkillCatalog 或 provider session；
- process restart 后所有 Session 都视为 unloaded；不恢复旧 stream、Tool task、approval waiter 或 AgentLoop state。

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
│  └─ SessionPrompts
└─ SessionStorage conversation

loaded Session execution
├─ exact SessionDefinition
├─ resolved Workspace state
├─ committed conversation projection
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
    pub status: AgentStatus,
    pub name: String,
    pub description: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}
```

Agent execution definition 是 immutable revision value：

```rust
pub struct AgentDefinition {
    pub agent_id: AgentId,
    pub revision: AgentRevision,
    pub prompts: AgentPrompts,
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

Agent disable/delete 不承担 security revocation。已完成 initiating UserMessage append 的 active Turn 使用不可变 Context，可以继续完成；需要立即停止时使用显式 Turn cancellation 或独立安全策略。

candidate admission在initiating UserMessage append前必须经过短Agent lifecycle synchronization。disable/delete与该append的胜负由该synchronization线性化，不能靠两个无保护状态读取猜测。

## Agent 操作

### Create Agent

```text
验证 AgentId 未使用
→ 验证 initial AgentDefinition
→ durable 写入 AgentRevision(1)
→ durable 发布 Agent head { Enabled, current = 1 }
```

create 必须原子可见。失败或 crash 的 staging definition 不进入 Agent catalog。

### Update Agent Definition

```text
expected current AgentRevision
+ candidate AgentDefinition
→ canonicalize / validate
→ no-op，或写入 revision N+1
→ 原子移动 current pointer
```

允许在Enabled或Disabled状态更新；Deleted返回terminal lifecycle error。definition/metadata update必须在Agent lifecycle synchronization内同时CAS expected AgentRevision和`status != Deleted`，因此不能在Delete线性化后发布迟到revision。

Agent update 不 fan-out 修改 Session，也不替换 active Turn Context。

### Update Agent Metadata

name/description 更新只改变 metadata 和 `updated_at`，不产生 AgentRevision。

### Disable / Enable / Delete

- Disable、Enable和Delete使用Agent lifecycle synchronization；
- 重复进入相同状态幂等；
- Deleted 不允许 Enable；
- operation outcome unknown 时按 operation id 查询 durable head，不能重复创建其他状态事实；
- loaded Sessions可以接收readiness invalidation，但initiating UserMessage append前的durable AgentStatus validation才是authoritative决策点。

## Session

Session head 保存 identity、当前 definition pointer、durable lifecycle 和用户可见 metadata：

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

Session 不直接重复保存 `agent_id`、Workspace、Model 或 SessionPrompts。这些 execution definition fields 属于 `SessionDefinition`：

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

```rust
pub struct SessionModelConfig {
    pub selection: ModelSelection,
    pub reasoning: ReasoningPreference,
    pub max_output_tokens: Option<NonZeroU32>,
}
```

SessionDefinition只保存stable selection和用户偏好，不保存provider endpoint、credential、capabilities或client。Turn admission通过`ModelGateway.resolve_for_turn(...)`把它解析为exact `TurnModelSnapshot`；catalog change只影响future Turn，active Turn不执行cross-model fallback。

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
WorkspaceSnapshot / authorization lease
SkillCatalog / LoadedSkill
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
SessionPrompts
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

active Turn、Steer、retry 和 compaction继续使用旧 capture；在 update 线性化点之后开始 admission 的 future Turn 使用新 revision。

loaded Session只能缓存current definition head作为可丢弃projection。definition update成功commit后，execution owner在同一per-session lifecycle synchronization内应用新的committed pointer；Turn admission仍以durable current head/CAS为准，不能因为loaded cache过期而继续创建旧revision的future Turn。

Workspace security restriction 是例外，分为两条 fail-closed 路径：

```text
definition-changing restriction
→ resolve/validate candidate
→ revoke 受影响的旧 authorization lease
→ durable commit 新 SessionDefinitionRevision
→ 通知 execution owner并 terminalize 受影响 active Turn
→ 发布 future WorkspaceSnapshot
→ 返回 update success

authority-only restriction
→ authority在自己的publication synchronization内原子发布新authority revision/policy并revoke旧lease
→ 通知 execution owner并 terminalize 受影响 active Turn
→ 使用 current exact SessionDefinition 重新 resolve
→ 发布受限的新 Snapshot，或 SessionReadiness = Unavailable
→ SessionDefinitionRevision 不变
```

若 definition-changing 路径在 revoke 后 commit 失败，旧 definition 仍是 durable current，但旧 lease 不得恢复；Session readiness 进入 Unavailable，等待从 durable truth 重新 resolve/repair。不能出现security update已对调用方确认而旧lease仍能通过model/tool/skill authorization validation的窗口。

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
+ target AgentRevisionRef
→ 按 Agent lifecycle → Session lifecycle 固定顺序获取 gates
→ target 必须属于同一个 AgentId
→ Agent 必须 Enabled
→ target definition 必须存在
→ 在 gates 内创建新的 SessionDefinitionRevision
```

调用方可以请求“升级到 current”作为 convenience，但提交前必须解析成 exact AgentRevisionRef；`latest` 不进入 durable SessionDefinition。

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

`Open` 表示 durable Session 可以执行，不表示当前已加载。

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
- lifecycle mutation 不隐式取消 active Turn；调用方先显式 cancel、等待 terminal、unload，再 archive/delete。

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
- committed conversation corruption；
- required Runtime dependency unavailable。

`SystemError` 不再是 durable Session lifecycle。需要修复的问题通过 `Unavailable(reason)`、load error 或 recovery diagnostics 表达。

Readiness 是 projection，不是 Agent/Session lifecycle admission 的 authoritative substitute。每次 initiating UserMessage append 前仍必须重新检查 durable SessionLifecycle、AgentStatus、exact revisions 和 Workspace authorization。

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
→ Starting          // admission reservation / Context capture / input append
→ Running           // initiating UserMessage entry appended
→ Finishing         // terminal entry / pending cleanup
→ Idle
```

失败路径：

```text
Starting failure → Idle
Running cancel/failure → Finishing → Idle
```

一个 loaded Session 同时最多一个 Starting 或 Running Turn。

这四个状态不进入 durable Session，也不替代 TurnStatus。

## Turn Status 与 Execution Phase

Turn durable status：

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
    PreparingModel,
    Sampling,
    WaitingApproval,
    ExecutingTools,
    Committing,
}
```

这些 phase 不进入 Turn durable status。

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
→ append tool_round_completed
→ PreparingModel
```

等待审批不是 Interrupted。Interaction 和 parent ToolInvocation 的完整语义见 [Turn、Item 与 Interaction 架构设计](turn-item-interaction.md)。

### Steer

Steer到达时Turn保持Running。请求必须携带`expected TurnId`，并在current-Turn validation中验证目标仍是同一个Running Turn：

```text
Steer(expected TurnId) accepted
→ current Turn control queue
→ 当前model/tool operation完成后append Steer
→ 下一次模型调用看见 Steer
```

默认情况下，WaitingApproval 中到达的 Steer 先排队，不自动当作审批结果。若 final terminal entry 先于 Steer acceptance 线性化，Steer 不再属于该 Turn；若 Steer 先被接受，final draft 必须先与 queued Steer 仲裁。

若产品选择让 Steer preempt approval：

```text
resolve pending Interaction as cancelled
→ 产生 cancelled ToolResult
→ append role=tool message
→ append tool_round_completed
→ append Steer
→ 同一个 Turn 继续 Running
```

只有显式 Turn cancel、runtime shutdown、security revocation 或不可恢复错误才使 Turn terminal。

## Create Session

```text
验证 Workspace / Model / SessionPrompts candidate
→ 获取Agent lifecycle synchronization
→ 最终检查 AgentStatus = Enabled
→ 在同一synchronization内读取current AgentRevisionRef
→ durable 写入 SessionDefinitionRevision(1)
→ durable 发布 Session { Open, current = 1 }
→ 确认publication outcome后释放synchronization
```

create 的 baseline 结果是 `Open + Unloaded`。create-and-load 是上层组合，不改变 create 的 durable 语义。

create 使用预分配 SessionId 和 operation id。outcome unknown 时必须查询已发布 target，不能创建重复 Session。

## Update Session Definition

只允许Open Session。普通definition update只能改变Workspace、Model或SessionPrompts；`AgentRevisionRef`变化必须走显式Agent upgrade路径，不能绕过Agent lifecycle synchronization。

```text
expected SessionLifecycle = Open
+ expected SessionDefinitionRevision
+ complete candidate definition
→ 在per-session lifecycle synchronization内validate / CAS
→ publish revision N+1
```

active Turn 不受 ordinary definition update 影响。FollowUp 在真正 admission 时读取 update 后的 current revision。successful update 必须让 loaded execution 的 current-definition projection 与 durable head 一致；若 projection apply 失败，Session 进入 Unavailable 并从 durable truth reload，不能悄悄继续使用旧 head。

metadata update与definition update分开，避免改标题导致execution Context fingerprint变化。metadata update可以作用于Open或Archived，但必须在per-session lifecycle synchronization内CAS `lifecycle != Deleted`和metadata version。

## Load Session

load 语义：

```text
在per-session residency/lifecycle synchronization内确认SessionLifecycle = Open
→ single-flight 标记 Loading，并 capture current SessionDefinitionRevision
→ 读取 Agent durable head / AgentStatus
→ 读取 exact AgentRevisionRef 对应的 AgentDefinition
→ 重建 committed conversation projection
→ WorkspaceResolver::resolve(definition.workspace)
→ 注入 Runtime Prompt/Tool/Skill/Model dependencies
→ 重新进入synchronization，CAS lifecycle仍为Open且current revision未变化
→ 原子发布 loaded execution state
```

若 final CAS 发现 SessionDefinitionRevision 已变化，必须丢弃旧 resolve 结果并按新 revision 重试或返回 retryable stale error，不能把旧Workspace/SessionModelConfig/Prompt projection 发布为 current loaded state。

重复 load 幂等返回同一个 loaded execution owner/handle。

Agent Disabled/Deleted 时可以加载 history projection，但 readiness 为 Unavailable，新 Turn admission 被拒绝。

exact AgentRevision 缺失或定义损坏时不能替换为 Agent current revision。history 若仍可读取，可以返回 read-only diagnostics；execution fail closed。

load 不恢复：

```text
provider stream
AgentLoop state
Tool task
approval waiter
cancellation token
old WorkspaceSnapshot / authorization lease
PromptSet / ToolSet / SkillCatalog
```

## Unload Session

unload 不修改 durable Session lifecycle、SessionDefinitionRevision 或 conversation。

前置条件：

```text
SessionExecutionState = Idle
无 admission reservation
无 active Turn
无 pending Interaction
当前没有正在执行的entry append
```

不满足时返回 Busy。调用方必须显式 cancel/resolve，并等待 terminal append 完成。

```text
Loaded → Unloading
→ drop resolved Workspace state
→ drop hot conversation projection
→ drop execution owner/runtime handles
→ Unloaded
```

重复 unload 幂等。

Runtime shutdown 使用：

```text
stop new admission
→ cancel active Turn
→ wait terminal append
→ unload
```

## Archive / Unarchive / Delete

### Archive

```text
Open + Unloaded
→ Archived
```

archive 不隐式 unload，也不取消 Turn。

### Unarchive

```text
Archived
→ Open + Unloaded
```

Workspace 和 exact Agent revision 在后续 load/admission 时重新验证。

### Delete

```text
Archived + Unloaded
→ Deleted
```

delete 不级联删除 Agent、Workspace files、fork children 或 sibling Sessions。

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

本阶段只定义 ForkSession lifecycle 语义。

前置条件：

- source 为 Open 或 Archived；
- source 不为 Deleted；
- public boundary是genesis，或ConversationStorage定义的UserMessage/final AssistantMessage前后anchor；Compaction本身不作为MVP公开anchor，位于selected message path上的Compaction自然随path生效；
- selected prefix可以包含至多一个non-terminal tail Turn；target staging必须以`HistoricalFork`原因关闭它；
- stream draft不属于durable prefix；若selected prefix含Pending Interaction、Started ToolInvocation或conversation-hidden incomplete ToolRound，closure entries必须在target staging中按truthful state resolve/abandon/interrupt，不能promotion或恢复执行；
- source SessionDefinition 和 exact AgentRevision 仍可读取；
- Agent 必须 Enabled，才能发布 Open child Session。

fork capture：

```text
source SessionDefinition at fork linearization point
+ committed stable conversation checkpoint
+ lightweight source provenance
```

child：

```text
new SessionId
SessionDefinitionRevision(1)
exact copy of source AgentRevisionRef
copy Workspace semantic fields，但分配 child-local WorkspaceRevision(1)
copy source Model / SessionPrompts
Open + Unloaded
independent future revisions
independent WorkspaceSnapshot / lease
independent conversation branch
```

不复制：

```text
source SessionDefinitionRevision / WorkspaceRevision number
loaded execution state
WorkspaceSnapshot / authorization control
ToolSet / PromptSet / SkillCatalog
provider session
pending Interaction / FollowUp queue
Session-scoped Tool grant
```

fork使用staging + atomic publication。copy/remap完成后，若target projection仍有Running Turn，则先逐entry append InteractionResolved/ToolAbandoned和`TurnInterrupted(HistoricalFork)`；不得补synthetic ToolResult或`tool_round_completed`。full replay确认无Running/Pending/Started后，child publication才可在Agent lifecycle gate内最终检查AgentStatus = Enabled，并与Agent disable/delete线性化；失败或crash的staging target不进入Session catalog。

Conversation/SessionStorage使用`EntryId + parent_id` entry tree；fork deep-copy selected path，并remap EntryId、TurnId、ItemId和RequestId，preserve ToolCallId与exact content/definition references。完整规则见[Conversation 与 SessionStorage 架构设计](conversation-storage.md)。

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
→ capture SkillCatalog / ToolSet / PromptSet
→ 获取短Agent lifecycle synchronization
→ 最终检查 AgentStatus = Enabled
→ 在gate内append TurnContext和initiating UserMessage，并确认outcome
```

TurnContext entry至少保存：

```text
SessionDefinitionRevision
AgentRevisionRef
Workspace/Prompt/Tool/Skill/Model exact fingerprints/references
ExecutionContextFingerprint
```

Agent current revision不参与Turn capture，因为Session已经pin exact AgentRevisionRef。Agent status的authoritative check与initiating UserMessage append使用同一个短lifecycle synchronization；capture前的status/readiness check只用于提前失败。

## Active 与 Future Turn

| 操作 | candidate admission | active Turn | future Turn |
| --- | --- | --- | --- |
| Agent 发布新 revision | 保持 exact Session pin | 不变 | 仍使用 Session pin，直到显式升级 |
| Session 显式升级 Agent | 已捕获旧 SessionDefinitionRevision 者不变 | 不变 | 使用新 AgentRevisionRef |
| Session definition ordinary update | 已捕获旧 revision 者不变 | 不变 | 使用新 SessionDefinitionRevision |
| Agent disable/delete | input append前的synchronization决定胜负 | 已开始Turn继续 | admission拒绝 |
| Session archive/delete | 要求先无 candidate | 要求先无 active Turn | lifecycle 状态拒绝 |
| ordinary Workspace update | 已捕获旧 basis 者不变 | 不变 | 使用新 definition/snapshot |
| restrictive Workspace update | lease/final check 失败 | revoke 并中断 | 使用新 Snapshot |
| unload | Starting/Running 时 Busy | 不允许 | load 后重新 admission |

## 并发线性化

### Definition Update vs Turn Admission/Load

Session admission synchronization保证candidate得到完整旧SessionDefinition或完整新SessionDefinition，不允许字段跨revision混合。Loading使用captured revision构建临时状态，并在publication前CAS current head；definition update先赢时旧load result被丢弃。

### Agent Disable/Delete vs Turn Start Append

短Agent lifecycle synchronization决定：

```text
status mutation先赢
→ candidate input append被拒绝

initiating UserMessage append先赢
→ Turn 使用 pinned Context 继续
```

Agent lifecycle gate从final Enabled check持有到initiating UserMessage append outcome确认；active Turn不持续持有该gate。CreateSession、Session Agent upgrade和Fork child publication也使用同一gate，保证disable/delete先赢时不会发布新的可执行引用。

### Session Definition/Metadata Update vs Archive/Delete

Session definition update必须在per-session lifecycle synchronization内同时CAS `SessionLifecycle::Open`与expected revision；metadata update CAS `lifecycle != Deleted`与expected metadata version。Archive/Delete先赢时不满足该operation前置条件的迟到update失败；update先赢时lifecycle mutation观察新的durable head。

跨Agent/Session操作使用固定synchronization顺序`Agent lifecycle → Session lifecycle`，避免Agent disable、Session upgrade/fork和archive之间形成锁环。

### Load vs Load

同一 Session load single-flight；不能创建两个 execution owners。

### Load/Admission vs Unload/Archive/Delete

使用同一个per-session residency/lifecycle synchronization串行化。不能让旧loaded state和新loaded state同时存在。

### Entry Append vs Cancel/Unload

append一旦进入physical write不可被run cancellation中断。cancel/unload等待结果，再完成terminal handling。

## Crash Recovery

Runtime restart：

```text
所有 durable Agent/Session/definition/conversation 保留
所有 loaded Session execution state 消失
→ SessionLoadState = Unloaded
```

显式 load 时：

1. 读取 Session durable head；
2. 校验 lifecycle；
3. 读取 current exact SessionDefinitionRevision；
4. 读取 exact AgentRevisionRef；
5. 重建 committed conversation；
6. 重新 resolve Workspace；
7. 检测没有final AssistantMessage、TurnInterrupted或TurnFailed entry的旧Running Turn及其pending/open state；
8. baseline使用稳定operation key逐entry append InteractionResolved、ToolAbandoned和TurnInterrupted；已有role=tool message的ToolInvocation保持Completed，但不补做ToolRound completion；
9. 已有terminal entry但仍遗留Pending Interaction或Started Item属于semantic corruption，read-write load fail closed并要求显式repair，不能追加“修复closure”掩盖历史；
10. 进入 Ready、Unavailable 或返回 typed load/corruption error。

禁止：

- 用 Agent current revision 替代 Session pin；
- 用current SessionDefinition替代旧TurnContext entry引用的exact definition；
- 恢复旧 provider stream、AgentLoop、Tool task 或 approval waiter；
- 自动重放 outcome unknown 的非幂等 Tool；
- 用 last-good WorkspaceSnapshot 绕过当前 authority。

## 错误分类

本阶段只固定语义分类，不冻结公开 error enum：

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
DurableStateCorrupt
IdConflict
OutcomeUnknown
```

重试建议：

- stale/busy/outcome unknown：重新读取 durable truth 后决定；
- disabled/archived：需要显式 enable/unarchive；
- deleted/invalid transition：普通流程不可恢复；
- unavailable：修复 Workspace/dependency 后 retry load；
- corrupt/revision unavailable：fail closed，进入 repair/audit。

## 方案比较

### Session 跟随 Agent Current

优点：类型和操作少；Agent 更新自动影响所有 Session future Turn。

缺点：同一 Session 行为无感漂移；fork 首个 Turn 可能与 source 不同；更新 blast radius 大；recovery 仍必须额外保存历史 exact revision。

### Session Pin Exact Agent Revision

优点：SessionDefinition 自包含；future Turn、fork、审计和 recovery 可解释；Agent update 不产生隐式 fan-out。

缺点：需要显式升级；旧 Session 可能长期停留旧 revision；revision retention 成本更高。

### Agent Stable/Preview Channel

优点：支持 staged rollout、promotion 和 rollback。

缺点：增加 channel generation、promotion CAS、fork 漂移、GC roots 和 UI 状态。当前没有足够产品需求。

### 决策

MiniCore 使用 exact pin：

```text
SessionDefinition.agent = AgentRevisionRef
```

不同时支持 follow-current/pinned 两种模式，也不引入 Agent channel。未来若出现真实 release workflow，channel 只能作为创建或显式升级 Session 时解析 exact revision 的 convenience alias，不能成为 Session 的持续动态绑定。

## 与三个 Service 的关系

PromptService、ToolService 和 SkillService 都由 MiniCoreRuntime 创建并在 Runtime 生命周期内共享。

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

- Agent 是 durable reusable definition owner，不是 running AgentLoop；
- Agent revision immutable，old revision 不被 current update 覆盖；
- Agent status 与 AgentRevision 正交；
- Session pin exact AgentRevisionRef；
- Agent current update 不自动改变 Session；
- Session 只能升级同一个 AgentId 的 revision；
- SessionDefinitionRevision原子绑定AgentRevisionRef、Workspace、SessionModelConfig和SessionPrompts；
- Session 不持有 Runtime Service handle；
- Session durable lifecycle 与 load/readiness/execution state 分离；
- Open 不等于 Loaded；Loaded 不等于 Ready；Ready 不等于 Running；
- Deleted 是逻辑删除，Purge 才是物理清除；
- archive/unload/delete 不使用同一个 close 语义；
- active Turn 使用 captured exact definitions，不受 ordinary update 影响；
- Agent disabled/deleted与CreateSession、upgrade、fork publication和initiating UserMessage append使用同一lifecycle synchronization线性化；
- Agent disabled/deleted 阻止 future admission，但不 patch active Context；
- Workspace restrictive update仍可 revoke active Turn；
- WaitingApproval 时 Turn 仍是 Running；
- Steer 不把 Turn 变成 Interrupted；
- fork从genesis或公开message anchor创建；mid-Turn prefix在target staging中以HistoricalFork中断后才发布；
- fork 不复制 loaded execution state或 authorization capability；
- host restart 后 loaded state 全部丢失，可由 durable truth 重建；
- recovery 不使用 current revision 冒充历史 exact reference；
- recovery terminalization、pending Interaction closure 和 Started ToolInvocation closure 使用各自稳定 operation key 逐 entry 幂等追加。

## Test Matrix

至少覆盖：

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
- Workspace update 同时改变 WorkspaceRevision 和 SessionDefinitionRevision；
- Open/Archived/Deleted transition；
- archive loaded Session 返回 conflict；
- delete 非 Archived/loaded Session 返回 conflict；
- load single-flight，且 definition update 先赢时旧 load publication CAS 失败；
- load exact Agent revision，不用 current 替代；
- Workspace unavailable 形成 Loaded + Unavailable；
- unload Busy 与幂等 unload；
- Session execution Idle/Starting/Running/Finishing；
- WaitingApproval 保持 Turn Running；
- Steer 在 WaitingApproval 时排队；
- preempt approval 时 cancelled ToolResult + Steer 仍保持同一 Turn；
- Agent disable/delete vs initiating UserMessage append final synchronization；
- CreateSession/Agent upgrade/Fork publication vs Agent disable/delete；
- Session definition/metadata update vs archive/delete lifecycle synchronization；
- Session definition update vs admission 捕获完整旧/新 revision；
- entry append vs cancel/unload；
- fork Open/Archived source，覆盖terminal boundary与mid-Turn message anchor；
- fork 复制 exact AgentRevisionRef 与 definition content，但创建 child-local WorkspaceRevision(1)；
- fork 不复制 Snapshot/lease/ToolSet/PromptSet/SkillCatalog；
- fork staging crash不发布target；mid-Turn fork closure不恢复source执行状态；
- restrictive Workspace definition update revoke 后 append 失败时保持 fail closed / Unavailable；
- authority-only restriction 不创建 SessionDefinitionRevision；
- Steer expected TurnId 与 final terminal append race；
- restart 后所有 Session Unloaded；
- incomplete Turn 只 terminalize 一次；
- recovery 逐 entry terminalize Turn、关闭 pending Interaction并保留已有 tool message或Abandon Started ToolInvocation；
- 已 terminal Turn 遗留 pending Interaction 或 Started Item 时 fail closed并进入 explicit repair；
- missing exact revision fail closed；
- Deleted identity 不复用；
- Purge 不属于普通 lifecycle。

## 后续问题

1. AgentDefinition 未来是否持有 Tool/Skill/Model constraints。
2. Agent/Session immutable definition 的具体 persistence schema。
3. Agent/Session entity head 的 operation id、CAS 和 durable store 形状。
4. Session list 如何投影 load/readiness/execution state。
5. auto-unload policy、idle timeout 和 subscription 对 residency 的影响。
6. physical purge、retention 和 revision reachability GC。
7. 多进程同时操作同一个 Agent/Session store 的并发实现。
8. SessionExecutionHandle如何映射到阶段9公开Runtime protocol。
9. public command/query/event/snapshot lifecycle payload。

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
- [x] 定义 WaitingApproval、Steer 和 Interrupted 的关系。
- [x] 定义 conservative crash recovery。
- [x] 完成 operation-centric Item、durable Interaction 和 terminal cleanup 类型。
- [x] 完成 Session ledger identity、entry parent tree、fork remap 和 append contract。
- [x] 完成SessionExecutor owner和crate-private request interface。
- [ ] 完成公开Runtime interface。
