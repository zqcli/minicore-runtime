# Turn Execution Context 架构设计

状态：当前权威架构（ADR 0124后，生产实现待启动）
日期：2026-07-29

## 目的

本文定义一次Turn的immutable execution binding、capture依赖、Prompt/AgentLoop接口、Model/Tool/Interaction/Steer/Compaction连接，以及restart后不恢复旧execution context的规则。

核心区分：

```text
Turn
→ durable domain lifecycle

TurnExecutionContext
→ current Runtime内一次Turn捕获的immutable execution objects

StoredTurnStart
→ Input UserMessage内联的safe historical metadata
```

## 决策摘要

- TurnExecutionContext在admission时一次性capture；
- active Turn固定exact AgentRevisionRef、SessionDefinitionRevision、WorkspaceSnapshot、SkillView、ToolSet、PromptSet和TurnModelSnapshot；
- private constructor阻止跨capture拼接；
- shared resource只在显式reload后影响future Turn；
- Input UserMessage内联StoredTurnStart，不写独立TurnContext entry；
- durable history不保存WorkspaceRevision/ModelDefinitionVersion execution ref；
- cold replay不重建旧PromptSet/ToolSet/SkillView/WorkspaceSnapshot；
- PromptSet是唯一模型上下文组装seam；
- AgentLoop只接收validated model response和typed committed deltas；
- Tool side-effect start是SessionExecutor current-Runtime状态；
- complete Tool exchange由matching ToolResult自动形成；
- Steer复用同一个TurnExecutionContext；
- FollowUp创建新Turn和新Context；
- Compaction使用single prefix marker，Replace后重建AgentLoop；
- restart关闭旧Running Turn，不exact resume。

## 三层边界

### 领域 Turn

```text
Input UserMessage append
→ Running
→ Final Assistant | TurnInterrupted | TurnFailed
```

### Turn Execution

```text
admission reservation
→ capture TurnExecutionContext
→ Input append
→ AgentLoop/Model/Tool/Interaction/Compaction
→ terminal
```

### AgentLoop

```text
NeedModel | NeedTools | Finished
```

AgentLoop不拥有Prompt、Tool、storage、Steer queue、Cancel或terminal arbitration。

## 对象关系

```text
MiniCoreRuntime current shared roots
+ Session current definition
+ Agent exact definition
+ Workspace current snapshot
+ candidate TurnId

→ ModelGateway.resolve_for_turn
→ SkillService.for_turn
→ ToolService.for_turn
→ PromptService.for_turn
→ Arc<TurnExecutionContext>
```

所有对象来自同一次capture。active Turn持有Arc后，reload不能原地替换它们。

## TurnExecutionContext

```rust
pub(crate) struct TurnExecutionContext {
    session_id: SessionId,
    turn_id: TurnId,
    session_revision: SessionDefinitionRevision,
    agent: AgentRevisionRef,
    model: Arc<TurnModelSnapshot>,
    workspace: Arc<WorkspaceSnapshot>,
    skill_view: Arc<SkillView>,
    tool_set: Arc<ToolSet>,
    prompt_set: Arc<PromptSet>,
    compaction: CompactionSettingsSnapshot,
    diagnostics: Arc<[TurnContextDiagnostic]>,
}
```

字段private，只提供窄方法：

```rust
impl TurnExecutionContext {
    pub fn compose_input(
        &self,
        intent: UserMessageIntent,
    ) -> Result<CanonicalUserMessage, PromptError>;

    pub fn compose_steer(
        &self,
        intent: UserMessageIntent,
    ) -> Result<CanonicalUserMessage, PromptError>;

    pub fn assemble_model_context(
        &self,
        input: PromptAssemblyInput<'_>,
    ) -> Result<AssembledModelContext, PromptError>;

    pub fn stored_turn_start(&self) -> StoredTurnStart;

    pub fn tool_set(&self) -> &Arc<ToolSet>;
    pub fn model(&self) -> &Arc<TurnModelSnapshot>;
    pub fn checkpoint_policy(&self) -> &CompactionSettingsSnapshot;
}
```

不暴露可替换字段或任意constructor。

## Context Capture

capture输入：

```text
Submit CommandId
candidate TurnId
SessionId + current exact SessionDefinitionRevision
SessionDefinition.agent = exact AgentRevisionRef
current WorkspaceSnapshot
atomic-cloned shared Prompt/Skill/Tool/Model roots
```

`SessionDefinitionRevision`保证Agent selection、Workspace definition、SessionModelConfig和Prompt selection来自同一个current definition。active Turn仍依赖exact in-memory values；durable replay不要求旧definition永久存在。

## Capture 依赖图

```text
Session definition + Agent definition
+ WorkspaceSnapshot
+ SharedResourceRoots

├─ ModelGateway.resolve_for_turn
│  └─ Arc<TurnModelSnapshot>
├─ SkillService.for_turn
│  └─ Arc<SkillView>
├─ ToolService.for_turn
│  └─ Arc<ToolSet>
└─ PromptService.for_turn
   ├─ Workspace prompt context
   ├─ Skill prompt view
   ├─ Tool prompt view
   └─ Arc<PromptSet>

→ private TurnExecutionContext::new(...)
```

ToolPromptView只能由同一个ToolSet投影；SkillPromptView只能来自同一个SkillView；PromptSet不能由caller拼接任意view。

## Capture 线性化

```text
reserve admission slot + candidate TurnId
→ capture current SessionDefinitionRevision
→ check Agent enabled and resolve exact AgentRevisionRef
→ clone all shared roots under one short publication gate
→ use current WorkspaceSnapshot
→ build all per-Turn objects
→ revalidate Session still Idle/Starting candidate current
→ compose Input
→ append Input + StoredTurnStart
```

若capture期间Session definition或Workspace publication改变，丢弃candidate并retry/return stale。capture成功不代表Turn已开始；Input append才是领域线性化点。

## StoredTurnStart

```rust
pub struct StoredTurnStart {
    pub agent_id: AgentId,
    pub agent_revision: AgentRevisionRef,
    pub session_definition_revision: SessionDefinitionRevision,
    pub model: StoredModelDescriptor,
    pub workspace: StoredWorkspaceSummary,
    pub diagnostics: Arc<[StoredContextDiagnostic]>,
}
```

它是safe historical description：

- model保存实际provider/model和generation settings；
- workspace只保存model-safe cwd/root display；
- exact Agent/Session revision为当前MVP必填历史说明；
- 不保存authorization capability、credential、endpoint或process-local handle；
- cold replay不重新resolve这些refs；
- future Turn使用current definition重新capture。

删除：

```text
StoredTurnContext entry
context_entry_id
WorkspaceSnapshotRef
historical WorkspaceRevision requirement
historical ModelDefinitionVersion requirement
```

## Admission

```text
Idle
→ candidate reservation
→ Starting
→ capture Context
→ compose Input
→ controlled append/apply Input(turn_start=Some)
→ Running
→ AgentLoop.from_seed(current conversation)
```

失败：

- capture/compose失败：无Turn；
- append NotCommitted：可retry同一draft；
- append OutcomeUnknown：writer poison，本run不retry；下次load按文件实际内容决定是否存在Turn。

## Prompt 与 Transcript-First

PromptSet assembly只接受：

- TurnExecutionContext captured static objects；
- sanitized `CommittedConversationView`；
- typed ModelCallPurpose；
- OutputContract；
- optional Compaction directive/budget。

禁止输入：

```text
raw message Vec
streaming buffer
execution-local ToolResult
uncommitted Steer
orphan/incomplete Tool exchange
arbitrary current-call contribution
```

SessionStorage是已写入history的来源；Prompt在模型调用前保证ToolCall/ToolResult协议完整。

## 逻辑模型调用

```rust
pub struct ModelCallRequest {
    // immutable provider-neutral request
}
```

每次logical call：

```text
current ConversationCheckpoint
+ TurnExecutionContext
+ purpose/output contract
→ PromptSet.assemble
→ ModelCallRequest::new
→ Arc<ModelCallRequest>
```

logical retry复用同一个Arc request，不重新assemble。retry前验证Turn、execution_version、checkpoint、current operation和control basis未变。

## AgentLoop Contract

AgentLoop从committed conversation seed构造。

```text
NeedModel
→ SessionExecutor assembles/calls ModelGateway
→ accept_model_response

NeedTools
→ assistant append
→ ToolSet execution
→ matching Tool messages append
→ last matching result produces CommittedToolExchangeDelta
→ accept_committed_tool_results

Finished
→ SessionExecutor chooses Final or Continue+Steer
```

Compaction Replace后必须由SessionExecutor从new seed重建AgentLoop。Steer路径固定调用`accept_committed_steer`；该方法内部可增量推进或通过private helper等价重建segment。

## Turn Execution Loop

```text
Input committed
→ AgentLoop NeedModel
→ optional Compaction
→ ModelGateway
→ Assistant response
├─ ToolCalls
│  → append Assistant Intermediate
│  → execute Tools
│  → append matching Tool messages
│  → complete exchange delta
│  → optional Steer
│  → NeedModel
└─ no ToolCalls
   → queued Steer?
      ├─ yes: append Assistant Continue + Steer → NeedModel
      └─ no: append Assistant Final → Completed
```

## Tool Execution

ToolSet接收：

```rust
pub struct ToolExecutionRequest<'a> {
    pub item_id: ItemId,
    pub call: &'a ToolCall,
}
```

流程：

```text
schema/hook/policy
→ optional Interaction
→ Sandbox/permission validation
→ SessionExecutor owner-local start reservation
→ executor
→ ToolExecutionOutcome
→ append Tool message or ToolAbandoned
```

side-effect start不写ledger。ToolSet不能直接append conversation或推进AgentLoop。

## Waiting Approval

```text
ToolAuthorization = Ask
→ append InteractionRequested
→ notify host
→ phase WaitingApproval
→ append InteractionResolved
→ wake Tool continuation
```

等待期间Context仍immutable；Steer排队；Cancel/SecurityRevoked可关闭Interaction和Turn。

## WaitingForUserInput

首版ask-user route：

- 在file mutation ticket和side effect前调用；
- 不持有mutation permit、TurnControl reservation或lifecycle guard；
- 同一assistant step sibling calls尚未启动；
- answer成为PreExecution ToolResult；
- complete exchange后才开始下一次Model。

## Steer

Steer复用current Context，不重新resolveModel/Workspace/Tools/Skills。

```text
Steer enqueue(expected TurnId)
→ wait current model/tool/interaction/compaction safe completion
→ append UserMessage(source=Steer, turn_start=None)
→ accept committed steer delta
→ next logical model call
```

Steer不是approval answer，不抢占UserQuestion。

## FollowUp

FollowUp在current Turnterminal后：

```text
new candidate TurnId
→ new current definitions/shared roots/WorkspaceSnapshot
→ new TurnExecutionContext
→ new Input + StoredTurnStart
```

旧Context不能复用。

## Retry

### Model

- AgentRun 0–3 logical retries；
- CompactionSummary 0–1；
- Gateway attempt=1；
- same Arc request；
- old future result path关闭后再retry。

### Tool

- 不自动retry已经可能执行的Tool；
- exact ToolResult append失败不重新执行；
- outcome unknown→Abandoned；
- restart incomplete call不自动重放。

## Compaction

Compaction source来自sanitized committed conversation。

```text
pressure
→ plan contiguous prefix
→ summary request
→ StoredCompaction(summary + first_kept_entry_id)
→ append/apply Replace
→ rebuild AgentLoop
```

不使用：

```text
active instruction segment
protected EntryId set
previous checkpoint
coverage frontier
ConversationBoundary
```

MVP允许旧Input/Steer被summary覆盖。Context本身仍保持当前Turn exact执行对象，不因summary改变。

## Cancellation And SecurityRevoked

### Turn Cancellation

Cancel publish后立即ack。Context不再用于启动新operation。

- model/compaction可以drop；
- Prepared Tool不启动；
- Running Toolbest-effort cancel并settle；
- Pending Interaction关闭；
- appendTurnInterrupted；
- FollowUp等待旧Turnterminal。

### SecurityRevoked

- 停止新Model/Tool/source use；
- Running Toolsettle；
- terminal后重新resolveWorkspace；
- 不承诺撤销已打开fd或回滚副作用；
- start race由SessionExecutor current ToolOperationSlot决定，不依赖durable marker。

## Failure Atomicity

逐entry append，不提供跨entry事务：

- assistant已append、部分ToolResult已append时crash，history保留可见事实；
- complete exchange只有在全部matching results存在时进入model conversation；
- incomplete exchange隔离；
- terminal/Interaction cleanup可以部分成功；
- malformed中段记录由tolerant replay跳过；
- history可读与future Turn可admit是两个状态。

OutcomeUnknown不在本runreopen/replay-by-key。

## Exact Binding 与内部一致性

live Turn仍严格：

```text
same SessionId/TurnId
same SessionDefinitionRevision/AgentRevisionRef
same immutable WorkspaceSnapshot
same captured shared roots
same TurnModelSnapshot
same PromptSet/ToolSet/SkillView
current execution_version/checkpoint/operation/control basis
```

这些一致性由ownership、private constructor和same Arc保证。durable StoredTurnStart只作历史说明，不参与restart authorization或same-Turn recovery。

## Crash Recovery

restart：

- 不恢复TurnExecutionContext；
- 不恢复AgentLoop、ModelCallRequest、ToolSet execution、waiter或queue；
- SessionStorage tolerant replayhistory；
- complete exchange保留，incomplete exchange排除；
- Running Turnbest-effortInterrupted；
- future Turn重新capturecurrent exact objects；
- old refs无法解析不阻塞history read。

## Diagnostics 与释放

Context diagnostics可以包含：

- selected Agent revision；
- provider/model；
- Workspace root摘要；
- Prompt/Skill/Tool selection warning；
- model capability warning。

不能包含credential、raw endpoint、absolute unauthorized path、Sandbox internals或full provider payload。

Turn terminal/unload后释放Arc；shared old resource object在最后一个active Context释放后自然回收。

## 与 Session Execution 的关系

TurnExecutionContext提供immutable values和窄方法；SessionExecutor拥有：

- admission/terminal；
- current operation；
- Tool start reservation；
- append/apply；
- queue/control；
- retry/Compaction orchestration；
- Snapshot/Event。

Context不能直接dispatch command、publish event或修改SessionDefinition。

## 与同类项目的关系

- Codex在Turn开始固定cwd/model/tool context，公开Turn/Item ID；
- pi按run capture当前model/tools并使用entry tree；
- Gemini以prompt/session和callId关联Tool生命周期；
- OpenHands通过immutable event/action对象和conversation state驱动；
- MiniCore额外保留managed Agent/Session definitions，但不再要求durable transcript恢复旧execution bundle。

## 基础不变量

- 一个Turn一个immutable TurnExecutionContext；
- active Turn exact binding严格；
- StoredTurnStart内联Input，只保存safe metadata；
- restart不重建旧Context；
- PromptSet唯一组装model input；
- AgentLoop只消费validated response和committed typed delta；
- Tool side-effect start是current-Runtime owner state；
- complete Tool exchange才进入model conversation；
- Steer复用Context，FollowUp新建Context；
- Compaction Replace后重建AgentLoop；
- reload只影响future Turn；
- Cancel/SecurityRevoked停止新operation并settleRunning Tool；
- cold replay局部损坏不brick Session。

## Test Matrix

至少覆盖：

- capture全部对象来自同一revision/publication；
- definition/reload race丢弃stale candidate；
- Input StoredTurnStart round-trip；
- cold replay不resolve旧Workspace/Model revision；
- Steer复用Context；
- FollowUp捕获new Context；
- Prompt拒绝raw/incomplete Tool exchange；
- logical retrysame Arc request；
- Tool start reservation vs emergency；
- complete Tool exchange delta推进AgentLoop；
- partial results不推进；
- Compaction single marker Replace和segment rebuild；
- Cancel/Revoked late result丢弃或settle；
- restart不恢复Context；
- old shared Arc在active Turn结束前保持有效。

## 明确不建立

```text
StoredTurnContext entry
WorkspaceSnapshotRef in ledger
ModelDefinitionVersion in ledger Turn metadata
Execution fingerprint/generation
ModelStep / ModelAttempt entity
ToolExecutionStarted durable marker
ToolRoundCompleted durable marker
Turn-scoped mutable Service locator
same-Turn restart resume
```

## 开放问题

1. StoredTurnStart exact wire fields/casing；
2. model/workspace display metadata最小集合；
3. AgentLoop L2 one-shot emission/error contract；
4. future exact resume需求出现时另立ADR，不能复用当前history metadata冒充execution checkpoint。
