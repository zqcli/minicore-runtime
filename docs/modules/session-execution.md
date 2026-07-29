# Session Execution 架构设计

状态：当前权威架构（ADR 0124后，生产实现待启动）
日期：2026-07-29

## 目的

本文定义loaded Session的单owner执行模型、SessionIngress语义lane、AgentLoop驱动、Model/Tool/Interaction/Steer/FollowUp/Cancel/Compaction流程，以及宽容SessionStorage语义下的restart recovery。

核心目标：

- 每个loaded Session只有一个mutable execution owner；
- 多Session可以并行；
- control不被普通work queue饿死；
- live执行严格，storage replay宽容；
- Tool side-effect start由current Runtime owner管理，不依赖durable start marker；
- complete Tool exchange由matching ToolResult集合自动形成；
- old provider/Tool task不跨restart恢复。

## 决策摘要

- 每个loaded Session一个`SessionExecutor`、一个`SessionWriter`、一个current Turn和一个current `RunningOperation`；
- `SessionIngress`分离Submit、Steer、FollowUp、Interaction、Tool control和sticky lifecycle/emergency；
- AgentLoop是crate-private同步sans-I/O状态机；
- ModelGateway每次operation一个provider attempt；logical retry归SessionExecutor；
- Input UserMessage内联StoredTurnStart并开始Turn；
- Tool side-effect start是owner-local reservation与in-memory state；
- 不append`ToolExecutionStarted`；
- 不append`ToolRoundCompleted`；
- 最后一个补齐assistant call集合的Tool message产生`CommittedToolExchangeDelta`；
- AgentLoop只消费typed committed model response、Tool exchange和Steer delta；
- Cancel发布sticky epoch后立即返回`CancelAccepted`；
- 已开始Tool继续settle，旧Turn进入Finishing；
- FollowUp可以在Finishing排队，旧Turnterminal前不启动；
- cold replay局部损坏不brick Session；
- restart后Running Turn保守中断，incomplete Tool exchange不进入模型conversation；
- Prompt assembly保持同步纯内存操作。

## Runtime关系

```text
MiniCoreRuntime
└─ LoadedSessionExecutors
   ├─ SessionId A → SessionExecutionHandle A → SessionExecutor A
   ├─ SessionId B → SessionExecutionHandle B → SessionExecutor B
   └─ SessionId C → SessionExecutionHandle C → SessionExecutor C
```

Runtime不拥有一个全局active Session。UI当前选中Session只是Presentation状态。

## 核心对象

```rust
pub(crate) struct SessionExecutor {
    session_id: SessionId,
    writer: SessionWriter,
    projections: SessionProjections,
    ingress: SessionIngress,
    state: SessionExecutionState,
    current_turn: Option<CurrentTurnExecution>,
    current_operation: Option<RunningOperation>,
    published_snapshot: ArcSwap<SessionSnapshot>,
}
```

```rust
pub(crate) struct SessionExecutionHandle {
    session_id: SessionId,
    ingress: SessionIngressHandle,
    snapshot: SessionSnapshotHandle,
}
```

Handle不暴露writer、projection internals、AgentLoop、waiter或Tool executor。

## SessionIngress

```text
TurnAdmissionQueue      → Submit
SteerQueue<TurnId>      → Steer
FollowUpQueue           → FollowUp
InteractionControl      → ResolveInteraction
ToolControlQueue        → Tool callback/control request
EmergencyControl        → Cancel / SecurityRevoked sticky epoch
LifecycleControl        → PrepareForUnload / Shutdown sticky signal
Snapshot                → immutable published view
```

规则：

- lane内部FIFO；
- 不建立跨lane全局FIFO；
- 每次wakeup由state-aware arbitration选择可处理工作；
- EmergencyControl不等待普通bounded lane容量；
- stale TurnId/CommandId不能影响new Turn；
- Snapshot读取不排队到长Tool或provider I/O之后。

## Session Execution State

```rust
pub enum SessionExecutionState {
    Idle,
    Starting,
    Running,
    Finishing,
}
```

- Idle：可admit新Submit或FollowUp；
- Starting：已预留candidate TurnId，尚未append Input；
- Running：Turn已开始；
- Finishing：Cancel/SecurityRevoked/failure已停止新逻辑推进，正在settle operation和appendterminal。

Workspace definition update只在Idle接受。

## Current Turn Execution

```rust
pub(crate) struct CurrentTurnExecution {
    submit_command_id: CommandId,
    turn_id: TurnId,
    execution_version: u64,
    context: Arc<TurnExecutionContext>,
    agent_loop: AgentLoop,
    phase: TurnExecutionPhase,
    steer_queue: SteerQueue,
    tool_operations: HashMap<ItemId, ToolOperationSlot>,
    compactions_completed: u8,
}
```

```rust
pub enum TurnExecutionPhase {
    Sampling,
    ExecutingTools,
    WaitingApproval,
    WaitingForUserInput,
    Compacting,
}
```

phase是process-local observer状态，不持久化。

## Tool Operation Slot

```rust
pub(crate) enum ToolOperationSlot {
    Prepared {
        call: ToolCall,
    },
    Running {
        call: ToolCall,
        cancellation: ToolCancellationHandle,
    },
    Settling {
        call: ToolCall,
    },
    Terminal,
}
```

Tool start线性化发生在SessionExecutor owner内：

```text
validate current Turn/Item/call
→ observe latest EmergencyControl
→ reserve start in current slot
→ slot = Running
→ invoke executor future
```

不写durable `ToolExecutionStarted`。start reservation前Emergency获胜则Tool不执行；slot进入Running后Cancel只能best-effort并等待settlement。

## Running Operation

**Canonical cross-module invariant: INV-101.** 索引见[架构总览](../architecture.md#跨模块不变量索引)。

```rust
pub(crate) enum RunningOperation {
    Model {
        turn_id: TurnId,
        execution_version: u64,
        purpose: ModelCallPurpose,
        source_checkpoint: ConversationCheckpoint,
        request: Arc<ModelCallRequest>,
    },
    Tools {
        turn_id: TurnId,
        execution_version: u64,
        assistant_entry_id: EntryId,
        expected_calls: Arc<[ExpectedToolCall]>,
    },
    Compaction {
        turn_id: TurnId,
        execution_version: u64,
        source_checkpoint: ConversationCheckpoint,
        plan: Arc<CompactionPlan>,
        request: Arc<ModelCallRequest>,
    },
}
```

每Session最多一个current RunningOperation。Tool executor futures可以并行存在，但由一个Tools operation集合拥有并回传owner；它们不能直接写storage或推进AgentLoop。

旧operation terminal/remove或安全drop并关闭结果路径前，不启动logical retry或下一operation。

## AgentLoop Interface

```rust
pub(crate) struct AgentLoop {
    // concrete private state
}

impl AgentLoop {
    pub fn from_seed(seed: ConversationSeed) -> Result<Self, AgentLoopError>;

    pub fn next_action(&mut self) -> Result<AgentLoopAction, AgentLoopError>;

    pub fn accept_model_response(
        &mut self,
        response: FinalizedAssistantResponse,
    ) -> Result<(), AgentLoopError>;

    pub fn accept_committed_tool_results(
        &mut self,
        delta: CommittedToolExchangeDelta,
    ) -> Result<(), AgentLoopError>;

    pub fn accept_committed_steer(
        &mut self,
        delta: CommittedSteerDelta,
    ) -> Result<(), AgentLoopError>;
}

pub enum AgentLoopAction {
    NeedModel { output_contract: OutputContract },
    NeedTools {
        response: FinalizedAssistantResponse,
        calls: Arc<[ToolCall]>,
    },
    Finished { candidate: FinalizedAssistantResponse },
}
```

Tool delta只能来自SessionStorage对最后一个matching Tool message生成的trusted delta。AgentLoop不接收execution-local ToolResult vector。

`next_action()`one-shot emission、重复poll和typed error的精确契约仍由第三版评审L2冻结；本次ADR不提前关闭L2。

AgentLoop禁止：

- I/O；
- 调用ModelGateway/ToolSet/PromptSet；
- 写SessionStorage；
- 处理approval、Cancel、Steer queue或terminal；
- 接收uncommitted conversation。

## Append与Projection更新

SessionExecutor只消费ConversationStorage的append receipt和trusted delta；strict commit可见性由[INV-001](../architecture.md#跨模块不变量索引)定义，tolerant replay由[INV-002](../architecture.md#跨模块不变量索引)定义。本节只描述Executor在这些结果上的本地推进。

```text
SessionWriter.append(draft)
→ strict live validation
→ physical append
→ CommittedSessionEntry
→ storage-owned apply_committed
→ publish StateEvent / wake waiter / start next operation
```

append OutcomeUnknown：

- poison writer；
- 不在同一run reopen/replay；
- 进入Finishing；
- 下次load执行tolerant replay。

hot apply因资源/checkpoint故障失败时，丢弃hot projections并full replay；replay可以隔离损坏记录，不要求和live validator返回同一错误。

## Submit流程

```text
CommandRequest(Submit, CommandId)
→ TurnAdmissionQueue
→ Idle arbitration
→ reserve candidate TurnId
→ state = Starting
→ capture exact SessionDefinition/Agent/Workspace/shared resources
→ build Arc<TurnExecutionContext>
→ PromptSet.compose_user_message
→ build StoredUserMessage {
     source = Input,
     turn_start = StoredTurnStart { safe history metadata }
   }
→ controlled append/apply
→ Turn Running
→ state = Running
→ return TurnStarted { turn_id }
→ AgentLoop.from_seed(current committed conversation)
```

Input append是领域Turn开始线性化点。capture/context compose失败时不创建Turn。

`CommandId`只在当前Runtime定位in-flight Submit和pre-Turn Cancel，不进入ledger。

## Drive AgentLoop

```text
AgentLoop.next_action()
├─ NeedModel → pressure/Compaction or AgentRun
├─ NeedTools → append Assistant(intermediate) then execute Tools
└─ Finished → terminal/Steer arbitration
```

每次drive前验证：

- current Turn仍Running；
- execution_version一致；
- no winning EmergencyControl；
- writer可用；
- current operation为空。

## Model流程

```text
NeedModel
→ current CommittedConversationView
→ Compaction pressure check
→ PromptSet.assemble
→ ModelCallRequest::new
→ install RunningOperation::Model
→ ModelGateway.generate_model_turn
→ terminal result
→ revalidate Turn/version/checkpoint/operation/control
→ AgentLoop.accept_model_response
→ drive next action
```

ModelGateway每次operation最多一个provider attempt，SDK retry=0。

logical retry：

- AgentRun最多3次；
- 同一个immutable `Arc<ModelCallRequest>`；
- 旧future已terminal/drop且结果路径关闭；
- retry delay期间仍处理Emergency/Lifecycle/Interaction/Snapshot；
- source checkpoint、execution_version和current operation basis必须未变。

## Tool流程

### Start Exchange

```text
NeedTools { response, calls }
→ append/apply Assistant(Intermediate with ToolCall content)
→ ToolInvocation Items become Started
→ install RunningOperation::Tools
→ phase = ExecutingTools
→ ToolSet.execute batch
```

assistant entry在Tool执行前durable，但不依赖额外ToolRound marker。

### Prepare and Approval

每个call：

```text
validate schema
→ run PreToolUse hooks
→ resolve policy/requirements
→ optional Interaction approval/UserQuestion
→ Sandbox capability check
→ re-observe EmergencyControl
```

pre-execution deny/failure/cancel产生truthfulPreExecution ToolResult，不启动side effect。

### Side Effect

Tool start/Cancel/SecurityRevoked的canonical first-wins与settlement规则见[INV-401](../architecture.md#跨模块不变量索引)；本节只描述SessionExecutor如何安装和驱动owner-local slot。

```text
start reservation wins
→ ToolOperationSlot::Running
→ executor future
→ Completed(exact result) | Abandoned
```

file mutation仍使用Session-local canonical file queue。持有mutation permit时不得等待UserQuestion；ask-user发生在ticket reservation前。

### Commit Results

Tool futures可以逆序完成。complete exchange的model-visible判定和typed delta构造由ConversationStorage按[INV-003](../architecture.md#跨模块不变量索引)拥有；SessionExecutor只提交exact outcome并消费返回的delta。owner按结果到达逐个：

```text
exact ToolExecutionOutcome::Completed
→ append role=tool message
→ apply Item Completed
→ if this completes assistant call set:
     receive CommittedToolExchangeDelta
     AgentLoop.accept_committed_tool_results(delta)
     clear Tools operation
     consume at most one Steer before next Model

ToolExecutionOutcome::Abandoned
→ optional append ToolAbandoned
→ terminalize/interruption path
```

complete exchange判断使用assistant call order和`TurnId + ItemId + ToolCallId`。没有`tool_round_completed`append。

Cancel/SecurityRevoked在部分results之后到达：

- 不启动remaining Prepared Tool；
- Running Tool settle；
- 已存在Tool messages保留；
- complete call集合如果自然形成，则conversation projector可以将其加入conversation；
- Turn仍按Interrupted terminalize；
- incomplete exchange在cold/hot model view中排除。

## Interaction流程

```text
ToolExecutionControl.request_approval/request_user_question
→ SessionExecutor validates current Turn/Item
→ append InteractionRequested
→ apply
→ publish UI-safe StateEvent
→ phase WaitingApproval/WaitingForUserInput
→ host ResolveInteraction
→ validate expected Turn/Item/Request/family/key
→ append InteractionResolved
→ apply
→ wake Tool continuation
```

等待不持有TurnControl reservation、lifecycle guard或file mutation permit。Steer只排队。

## Steer流程

**Canonical cross-module invariant: INV-102.** 索引见[架构总览](../architecture.md#跨模块不变量索引)。

```text
Steer(expected_turn_id, CommandId)
→ validate current Running Turn
→ bounded FIFO enqueue
→ return Queued
```

消费点：

- complete Tool exchange进入conversation后；
- 无ToolCall candidate final保存为Assistant Continue后；
- Compaction Replace后重建AgentLoop再消费；
- WaitingApproval/UserInput期间不消费。

```text
pop one Steer
→ PromptSet.compose_user_message
→ append UserMessage(source=Steer, turn_start=None)
→ apply CommittedSteerDelta
→ AgentLoop.accept_committed_steer
→ next Model
```

Final reservation与Steer admission必须由owner原子排序。

## FollowUp流程

FollowUp进入独立FIFO，可以在Running/Finishing期间排队。旧Turnterminal后：

```text
state = Idle
→ admission arbitration
→ accepted FollowUp或external Submit
→ new TurnExecutionContext
→ new Input UserMessage
```

FollowUp不复用旧TurnContext、AgentLoop或Tool state。

连续优先规则保持：旧Turn不是由FollowUp启动时，已accepted FollowUp最多获得一次连续优先；随后有external Submit待决时优先external Submit。

## Completed Turn流程

AgentLoop Finished candidate：

```text
reserve final commit vs Steer admission
├─ Steer queue empty
│  → append Assistant(Final)
│  → apply Turn Completed
│  → state Idle
└─ Steer admitted
   → append Assistant(Intermediate Continue)
   → apply
   → consume one Steer
   → continue same Turn
```

Final Assistant不能含ToolCall。

## Cancel流程

```text
Cancel(Submit CommandId | TurnId)
→ validate active target
→ publish sticky cancel epoch
→ cancel model/tool token where applicable
→ return CancelAccepted { target, cancel_epoch }
```

立即response只确认control signal已发布。

Executor观察后：

- Starting：取消candidate，不创建Turn；
- Running：停止新Model/Tool/Compaction和Steer消费，state→Finishing；
- Prepared Tool：生成Cancelled result或Abandoned，不启动；
- Running Tool：best-effort cancel，等待exact outcome或Abandoned；
- Pending Interaction：appendCancelled resolution；
- append TurnInterrupted(UserCancelled)；
- state→Idle；
- FollowUp保留等待。

不承诺回滚已进入OS/provider的副作用。

## Authority Security Interruption

SecurityRevoked使用sticky EmergencyControl，流程与Cancel settlement相同，但terminal reason为SecurityRevoked。

signal获胜后：

- 不启动新Model/Tool/source read；
- 不消费Steer；
- Prepared Tool不启动；
- Running Tooltruthful settle；
- terminal后重新resolve Workspace；
- success Ready/Idle，failure Unavailable。

不依赖durable ToolExecutionStarted；owner-local slot决定当前run是否已经开始副作用。

## Failure流程

### Starting failure

capture、composition或Input append NotCommitted失败：不创建Turn，返回typed rejection。

Input append OutcomeUnknown：writer poison，candidate不在本run重试；下次load按文件实际内容决定Turn是否存在。

### Running failure

- model/compaction terminal failure：按retry policy后appendTurnFailed；
- Tool普通失败有truthful ToolResult时Turn可以继续；
- Tool outcome unknown、writer unavailable或invariant failure进入Finishing并appendInterrupted/Failed；
- terminal append失败时history仍可read-only恢复，Session admission可Unavailable。

### Projection failure

hot apply失败：丢弃hot state并tolerant full replay。局部corruption返回diagnostics；无法得到安全current execution basis时停止Turn admission，不隐藏全部history。

## Tool Retry

MiniCore不自动retry已经可能发生副作用的Tool。

- validation/policy前失败可以形成PreExecution result；
- executor明确返回retryable error仍作为ToolResult交给模型，由模型决定是否再次调用；
- append ToolResult失败不能重新执行Tool；
- restart不自动重放incomplete ToolCall。

## Context Overflow 与 Compaction

```text
NeedModel
→ pressure Soft/Hard
→ Compaction::plan(single prefix marker)
→ PromptSet assemble Summary request
→ RunningOperation::Compaction
→ ModelGateway
→ validate same plan/request/checkpoint/control
→ append StoredCompaction(summary + first_kept_entry_id)
→ apply Replace
→ rebuild AgentLoop from new seed
→ pressure recheck
```

没有active instruction segment、protected entries、previous checkpoint或coverage frontier。

单TurnCompaction受`max_compactions_per_turn`限制。同source checkpoint hard recovery最多一次；successful Replace后可以形成新basis。

## Reload

shared `/reload`原子替换Prompt/Skill/Tool/Model current roots，只影响future Turn。active Turn继续使用captured old Arcs。

`/reload workspace`与Workspace definition update要求Idle。Running/Finishing返回SessionBusy。

## PrepareForUnload

```text
LifecycleControl(PrepareForUnload)
→ stop new admission
→ grace period处理current control/resolution
→ deadline到达则Cancel active Turn
→ settle Running Tool
→ best-effort terminal append
→ close writer/executor
```

旧queue、waiter、operation和snapshot handle不跨reload恢复。

## Snapshot

SessionExecutor每次authoritative mutation后发布immutable SessionSnapshot。Snapshot读取不等待普通work lane。

Snapshot包含：

- Session readiness/execution state；
- current Turn/phase；
- ordered Items；
- Pending Interactions；
- queue摘要；
- replay diagnostics摘要；
- usage与safe diagnostics。

History中的orphan/incomplete Tool事实可以显示warning；model-visible conversation仍由sanitized projection决定。

## Progress Event

ProgressEvent可合并/丢弃：

```text
AgentMessage/Reasoning started/delta
Tool stdout/progress
model retry scheduled
```

StateEvent/Snapshot提供最终校正。ProgressEvent不写storage，不决定Turn状态。

## Ingress与Operation处理顺序

每次主循环iteration：

```text
1. observe EmergencyControl/LifecycleControl
2. collect current operation readiness
3. process Interaction control
4. process Tool control/outcomes
5. process state-eligible Steer/Submit/FollowUp
6. drive AgentLoop when no current operation
7. publish snapshot if changed
```

不持有短guard跨await。Model provider I/O、Tool execution、approval、UserQuestion、file mutation和observer notify期间零持有lifecycle/TurnControl短guard。

## Multi-Session并发

- 每Session独立Executor/Writer/Ingress/file mutation queue；
- shared ModelGateway并发安全，多个Session的provider request不经过Gateway本地permit或admission queue；
- 同Session同文件mutation FIFO；不同文件可并发；多文件/open-world Tool使batch Serial；
- 跨Session共享Workspace冲突由host/user通过worktree或独立Workspace处理；
- 不建立Runtime-global Session lock hierarchy。

## Error分类

```rust
pub enum SessionExecutionError {
    SessionBusy,
    SessionNotReady,
    TurnNotRunning,
    StaleTurn,
    InvalidState,
    Storage(SessionStorageError),
    Model(ModelCallError),
    Tool(ToolExecutionError),
    Interaction(InteractionError),
    Compaction(CompactionError),
    Invariant(SessionInvariantError),
}
```

raw adapter error在owner module归一化；SessionExecutor只决定Turn recovery/terminal policy。

## Recovery

物理扫描、局部corruption隔离与complete/incomplete exchange投影分别由[INV-002与INV-003](../architecture.md#跨模块不变量索引)定义；SessionExecutor只拥有unfinished Turn的保守terminalization和future admission状态。

cold load：

```text
SessionStorage tolerant replay
→ publish replay diagnostics
→ no Running Turn: Idle
→ Running Turn:
     do not restore AgentLoop/provider/Tool/waiter/queue
     best-effort cancel Pending Interaction
     preserve complete Tool exchanges
     exclude incomplete exchanges from model conversation
     append TurnInterrupted(HostRestart | RecoveryContextUnavailable)
→ Idle or Unavailable
```

replay malformed line、orphan、duplicate和invalid cross-reference不会brick整个Session。无法appendrecovery terminal时history仍可read-only读取，future admission按writer/readiness状态决定。

## Performance

- Prompt assembly同步纯内存O(n)；
- cold open完整O(n) replay；
- loaded Session切换不replay；
- no ProjectionSnapshot/checkpoint index；
- observer progress有界并可丢弃；
- Tool/Model futures不持有Session mutex；
- 没有真实性能数据前不增加offload/counter/observer机制。

## Test Matrix

### State与Admission

- Idle→Starting→Running→Idle；
- candidate取消不创建Turn；
- Input内联StoredTurnStart；
- duplicate in-flight CommandId；
- Workspace update非Idle返回Busy。

### AgentLoop与Model

- NeedModel/NeedTools/Finished驱动；
- typed committed Tool exchange delta；
- logical retry复用同一request；
- stale checkpoint/version/operation result丢弃；
- Compaction Replace后重建segment。

### Tool

- Assistant ToolCall先append；
- start reservation vs Cancel/Security双向race；
- 不写ToolExecutionStarted；
- 多Tool并发逆序结果；
- 最后matching result自动形成exchange；
- live duplicate result及ToolResult/ToolAbandoned冲突被strict writer拒绝；cold replay duplicate first valid wins、terminal outcome first valid wins，missing/orphan/abandoned-first result不推进AgentLoop；
- append result失败不重放Tool；
- outcome unknown→Abandoned；
- Session-local file mutation FIFO。

### Interaction

- request-before-notify；
- resolution-before-wake；
- Waiting状态处理Steer/Cancel/Snapshot；
- reconnect同RequestId；
- terminal cleanup first-wins。

### Steer与FollowUp

- Tool exchange完成后再消费一条Steer；
- final reservation race；
- Finishing允许FollowUp排队；
- 旧Turnterminal前不启动FollowUp。

### Cancel与Recovery

- CancelAccepted即时返回；
- Prepared Tool不执行；
- Running Toolsettlement；
- malformed中段entry后load继续；
- incomplete exchange隔离；
- terminal append失败history仍readable；
- restart不恢复old tasks。

## 协同实现顺序

阶段6–8继续作为一个vertical slice：

```text
AgentLoop core types
→ ScriptedProviderAdapter
→ PromptSet / ModelCallRequest
→ ModelGateway single attempt
→ SessionExecutor ordinary AgentRun
→ Tool exchange auto-completion
→ Compaction single-marker Replace
→ cancellation/retry/replay fixtures
→ Rig provider spike
```

## Diagnostics

至少暴露：

- current Session/Turn/phase；
- active operation purpose；
- queued Steer/FollowUp count；
- Running/Settling Tool count；
- retry count；
- replay warning count/categories；
- last storage/model/tool typed error。

不得暴露credential、raw headers、absolute unauthorized path或full provider payload。

## 明确不建立

```text
Runtime-global active Session
Arc<Mutex<SessionExecutor>> across await
Tool actor per call
Interaction actor per request
ToolRound entity / ToolRoundCompleted event
ToolExecutionStarted durable marker
ModelAttempt entity
RunId / ModelStepId
cross-Session resource lock manager
projection checkpoint/index
```
