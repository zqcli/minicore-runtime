# Session Execution 架构设计

日期：2026-07-16

状态：目标架构已确定；实现和Rig adapter待后续阶段完成

## 目的

本文定义MiniCore中loaded Session的执行模块，回答：

- 一个Session如何接受Submit、Steer、FollowUp、Interaction resolution和Cancel；
- 一个Turn如何从UserMessage开始，经过model → tools → model循环，最终进入Completed、Interrupted或Failed；
- Model、Tool和Context构造如何异步执行，同时保持Session控制请求可处理；
- SessionWriter、projection、模型可见conversation、外部副作用和UI事件如何保持确定顺序；
- 多个Session如何在同一个MiniCoreRuntime中同时Running；
- Cancel、timeout、retry、restart和迟到结果如何处理；
- 哪些对象是领域事实，哪些只是执行期状态。

本文不定义：

- ModelGateway的provider映射、auth、fallback和stream wire格式；
- Compaction的具体planning算法、summary格式和质量评估；本文只引用其执行契约；
- Runtime公开command/query/event/snapshot协议；其映射以[Runtime Interface](runtime-interface.md)为权威；
- Rig 0.40.0 adapter的最终具体类型；
- 操作系统线程、Tokio task或local task的最终部署方式。

相关权威文档：

- [Conversation与SessionStorage架构设计](conversation-storage.md)
- [Turn执行模块与执行上下文架构设计](turn-execution-context.md)
- [Turn、Item与Interaction架构设计](turn-item-interaction.md)
- [Tool子系统架构设计](tool-subsystem.md)
- [ModelGateway架构设计](model-gateway.md)
- [Compaction架构设计](compaction.md)
- [Agent与Session生命周期架构设计](agent-session-lifecycle.md)
- [Runtime Interface与公开协议架构设计](runtime-interface.md)

## 决策摘要

已经确定：

- 每个loaded Session拥有一个`SessionExecutor`；
- 一个MiniCoreRuntime可以同时运行多个`SessionExecutor`；
- 每个Session同时最多一个Starting或Running Turn；
- `SessionExecutor`是该Session执行期mutable state的唯一owner；
- 外部调用方只持有可克隆的`SessionExecutionHandle`，不能直接借用Executor状态；
- 外部请求通过bounded FIFO `SessionRequestQueue`进入Executor；
- Context构造、UserMessage composition、Model调用和Tool执行使用异步`RunningOperation`，Executor不等待完整操作后才处理请求；
- 每个异步结果携带`SessionId + TurnId + execution_version + OperationType`；
- Steer、Cancel、security revocation或Turn terminal后，旧execution version的Context/Model结果不能修改当前状态；
- Tool副作用已经开始时，迟到结果仍必须确认并保存，不能因为version变化而丢弃；
- Session execution驱动private AgentLoop；AgentLoop不拥有storage、Prompt assembly、Tool execution或Turn状态；
- SessionWriter append由Executor发起，receipt必须立即应用到全部required projections；
- initiating UserMessage append/apply后才允许第一次Model调用；
- assistant intermediate和tool messages只有在`tool_round_completed`append/apply后才进入模型conversation；
- Interaction request append/apply后才通知host；resolution append/apply后才恢复Tool执行；
- `tool_execution_started`append/apply后才允许外部副作用；
- FollowUp使用process-local bounded FIFO，当前不承诺crash-safe delivery；
- Progress event可以合并或丢弃，不能影响request处理和durable state；
- restart不恢复旧Context/Model/Tool异步操作，unfinished Turn按既有recovery规则终止。

## 同类项目结论

### pi

pi使用AgentSession包装一个内部AgentLoop：模型调用、Tool执行、Steer queue和FollowUp queue都由该loop推进。其内外两层循环简单有效：内层处理model → tools → model并读取Steer；外层在Agent原本结束时读取FollowUp。

MiniCore采用Steer/FollowUp分离和内外两层逻辑，但不采用以下部分：

- AgentLoop直接拥有working transcript；
- UserMessage未确认durable就调用模型；
- approval只使用内存hook；
- 没有统一Tool副作用前置记录；
- 没有explicit ToolRoundCompleted conversation规则；
- Abort后没有durable Turn cleanup。

### Codex

Codex使用长期Session/Thread状态和active Turn task，支持expected TurnId Steer、Interrupt、approval以及TurnContext/StepContext。

MiniCore采用单Session owner、active Turn identity和异步操作分离，但active Turn不重新读取future Prompt、Tool、Skill、Workspace或Model state。

### Grok Build

Grok Build把ACP session、sampling、chat state、Tool和persistence拆为多个actor，并使用prompt queue和interjection保持响应。

MiniCore采用异步Model/Tool执行和bounded queue，但不为每个职责建立独立owner，也不建立第二conversation log或未持久化的模型输入通道。

### Claude Code与Cursor

两者证明queued prompt、interrupt、approval、resume、checkpoint和compaction具有实际产品价值。内部实现不能完整验证，因此只作为行为参考。

## Runtime关系

```text
MiniCoreRuntime
├─ shared PromptService
├─ shared ToolService
├─ shared SkillService
├─ shared ModelGateway
└─ LoadedSessionExecutors
   ├─ SessionId A → SessionExecutionHandle A → SessionExecutor A
   ├─ SessionId B → SessionExecutionHandle B → SessionExecutor B
   └─ SessionId C → SessionExecutionHandle C → SessionExecutor C
```

`LoadedSessionExecutors`是Runtime private map，不是领域entity或公开registry。

多Session规则：

- 每个Session有独立request queue、writer、projections、current Turn和execution version；
- 多个Session可以同时Sampling、WaitingApproval或ExecutingTools；
- ModelGateway负责provider并发限制；
- ToolService使用canonical resource locks协调跨Session文件和外部资源冲突；
- WorkspaceSnapshot和TurnExecutionContext按Session/Turn独立；
- Runtime不能保存全局current Session、current cwd、current Model或current Turn；
- UI selection属于UI，所有Runtime请求和事件必须携带SessionId。

## 核心对象

```rust
pub(crate) struct SessionExecutor {
    session_id: SessionId,
    state: SessionExecutionState,
    accepting_requests: bool,
    request_queue: SessionRequestReceiver,
    writer: SessionWriter,
    projections: SessionProjections,
    candidate_turn: Option<CandidateTurnExecution>,
    current_turn: Option<CurrentTurnExecution>,
    running_operations: RunningOperationSet,
    interaction_deadlines: InteractionDeadlineSet,
    pending_steers: VecDeque<PendingSteer>,
    follow_ups: VecDeque<PendingFollowUp>,
    pending_responses: PendingRequestResponses,
    progress_events: ProgressEventPublisher,
}

#[derive(Clone)]
pub(crate) struct SessionExecutionHandle {
    session_id: SessionId,
    requests: SessionRequestSender,
}
```

`SessionExecutor`不进入领域模型，不持久化，也不通过MiniCoreRuntime公开。

`SessionExecutionHandle`只负责发送请求和接收typed response：

- 不保存Session状态；
- 不提供`&mut SessionExecutor`；
- 不提供通用closure执行；
- 不绕过MiniCoreRuntime的公开权限和路由；
- queue关闭后返回typed unavailable error。

## Session Request Queue

`SessionRequestQueue`是bounded FIFO。成功进入队列的请求按Executor接收顺序处理，不静默丢弃。

```rust
pub(crate) enum SessionRequest {
    Submit {
        submission_id: SubmissionId,
        intent: PromptIntent,
        response: oneshot::Sender<Result<SubmitResult, SessionExecutionError>>,
    },
    Steer {
        expected_turn_id: TurnId,
        intent: PromptIntent,
        response: oneshot::Sender<Result<SteerResult, SessionExecutionError>>,
    },
    FollowUp {
        intent: PromptIntent,
        response: oneshot::Sender<Result<FollowUpResult, SessionExecutionError>>,
    },
    ResolveInteraction {
        expected_turn_id: TurnId,
        request_id: RequestId,
        resolution: InteractionResolution,
        resolution_key: IdempotencyKey,
        response: oneshot::Sender<Result<ResolveInteractionResult, SessionExecutionError>>,
    },
    Cancel {
        target: CancelTarget,
        reason: TurnInterruption,
        response: oneshot::Sender<Result<CancelResult, SessionExecutionError>>,
    },
    WorkspaceAuthorizationRevoked {
        reason: SecurityRevocationReason,
    },
    ToolControl {
        request: ToolControlRequest,
    },
    PrepareForUnload {
        response: oneshot::Sender<Result<(), SessionExecutionError>>,
    },
    GetSnapshot {
        response: oneshot::Sender<SessionExecutionSnapshot>,
    },
}

pub(crate) enum CancelTarget {
    Submission(SubmissionId),
    Turn(TurnId),
}
```

Queue规则：

- queue capacity由Runtime config决定；
- queue满时发送方等待或得到`RequestQueueFull`，不能丢弃已有请求；
- `GetSnapshot`仍经过同一queue，因此snapshot与mutation具有明确处理顺序；
- `ResolveInteraction`和`Cancel`不能被progress event阻塞；
- public protocol是否立即返回queued acknowledgement留到protocol阶段；
- crate-private `Submit` response只在initiating UserMessage append/apply成功后返回`Started { turn_id }`；
- capture或append失败返回Rejected，不产生领域Turn；
- `FollowUp`在进入FollowUp queue后返回Queued；
- `Steer`返回Applied或Queued，取决于当前执行阶段。

长流程的response不会让request handler等待：

- Submit response保存在candidate Turn中，UserMessage append/apply后完成；
- Cancel response保存到terminal处理完成；
- PrepareForUnload response保存到Executor进入Idle；
- queued Steer/FollowUp返回process-local completion handle，用于报告Applied、Started或Rejected。

## Tool Execution Control

Tool执行使用crate-private `ToolExecutionControl`请求SessionExecutor完成durable操作：

```rust
pub(crate) trait ToolExecutionControl: Send + Sync {
    async fn request_approval(
        &self,
        item_id: ItemId,
        request: ToolApprovalRequest,
    ) -> Result<ToolApprovalDecision, ToolExecutionControlError>;

    async fn record_execution_start(
        &self,
        item_id: ItemId,
        intent: ToolExecutionIntentStamp,
    ) -> Result<EntryId, ToolExecutionControlError>;
}
```

实现把`SessionRequest::ToolControl`写入同一个`SessionRequestQueue`。Tool future发送request后等待typed response，但不持有SessionExecutor的锁或mutable reference。

Tool control request与Cancel、Steer和WorkspaceAuthorizationRevoked共享一个FIFO顺序。因此已经进入queue的Cancel/revocation不会被另一个独立queue中的execution-start request越过。

SessionExecutor处理Tool control request时必须重新验证：

- expected SessionId和TurnId；
- execution version；
- ItemId/ToolCallId仍属于current Running Turn；
- Turn没有Cancel或security revocation；
- Interaction状态和approval family匹配；
- Workspace authorization和Tool policy仍允许继续；
- required entry append/apply成功。

Tool control request和外部Cancel同时到达时，由SessionExecutor实际处理顺序决定结果：

- Cancel先处理：拒绝新的execution start；
- ToolExecutionStarted先append/apply：副作用可以开始，之后Cancel只能best-effort取消并等待/确认outcome。

## Session Execution State

沿用既有transient state：

```rust
pub enum SessionExecutionState {
    Idle,
    Starting,
    Running,
    Finishing,
}
```

状态转换：

```text
Idle
→ Starting
→ Running
→ Finishing
→ Idle
```

失败路径：

```text
Starting failure
→ Idle

Running cancellation/failure
→ Finishing
→ Idle
```

规则：

- `Idle`：没有candidate或active Turn；
- `Starting`：已保留candidate Turn identity，正在构造Context或写入initiating entries；
- `Running`：initiating UserMessage已经append/apply；
- `Finishing`：不再启动新的Model/Tool操作，正在完成terminal entries和释放资源；
- 一个Session不能同时存在两个Starting/Running Turn；
- WaitingApproval、Compacting、Sampling和ExecutingTools是Running Turn的执行阶段，不是Session durable state；
- state不写入SessionStorage。

## Current Turn Execution

Starting期间使用execution-local candidate：

```rust
pub(crate) struct CandidateTurnExecution {
    submission_id: SubmissionId,
    turn_id: TurnId,
    execution_version: u64,
    intent: PromptIntent,
    context: Option<Arc<TurnExecutionContext>>,
    response: oneshot::Sender<Result<SubmitResult, SessionExecutionError>>,
    cancellation: CancellationToken,
}
```

Candidate不是领域Turn。initiating UserMessage append前Cancel使用`CancelTarget::Submission(submission_id)`定位它。

```rust
pub(crate) struct CurrentTurnExecution {
    turn_id: TurnId,
    execution_version: u64,
    context: Arc<TurnExecutionContext>,
    agent_loop: AgentLoop,
    phase: TurnExecutionPhase,
    cancellation: CancellationToken,
    model_attempt: Option<ModelAttemptState>,
    tool_execution: Option<ToolExecutionState>,
    compaction: Option<CurrentCompactionState>,
    automatic_overflow_recovery_used: bool,
}
```

`execution_version`从1开始，在以下情况递增：

- Sampling阶段应用Steer并废弃旧Model request；
- Compaction成功append/apply并Replace conversation；
- Cancel；
- security revocation；
- Turn进入terminal处理。

递增version不能撤销已经append的entry，也不能丢弃已经可能发生副作用的Tool outcome。

## Running Operation

`RunningOperationSet`是`FuturesUnordered<RunningOperation>`或等价private实现。SessionExecutor使用它保存异步操作。每个execution version最多有一个current operation；已经取消的旧version operation可以继续存在，直到返回结果或确认取消。

```rust
pub(crate) enum RunningOperation {
    BuildTurnContext {
        turn_id: TurnId,
        execution_version: u64,
        cancel: CancellationToken,
    },
    ComposeUserMessage {
        turn_id: TurnId,
        execution_version: u64,
        source: UserMessageSource,
        cancel: CancellationToken,
    },
    GenerateModelResponse {
        turn_id: TurnId,
        execution_version: u64,
        cancel: CancellationToken,
    },
    ExecuteTools {
        turn_id: TurnId,
        execution_version: u64,
        cancel: CancellationToken,
    },
    CompactConversation {
        turn_id: TurnId,
        execution_version: u64,
        source: ConversationCheckpoint,
        plan_fingerprint: CompactionPlanFingerprint,
        cancel: CancellationToken,
    },
}
```

ToolSet内部可以并发多个互不冲突的ToolCall，但SessionExecutor只观察一个Tool执行组。

异步结果：

```rust
pub(crate) struct OperationResult {
    session_id: SessionId,
    turn_id: TurnId,
    execution_version: u64,
    operation_type: OperationType,
    output: OperationOutput,
}
```

结果处理规则：

- SessionId不匹配：实现错误，记录diagnostic；
- TurnId不匹配：忽略结果并记录diagnostic；
- execution version过期的Context/Model/Compaction结果：忽略；
- execution version过期的UserMessage composition结果：忽略；
- Tool尚未越过`ToolExecutionStarted`记录时，过期结果可以取消/忽略；
- Tool已经越过该记录时，必须确认outcome：exact result保存为Tool message，无法确认则ToolAbandoned；
- terminal Turn不能接受新的Model result、ToolCall或Interaction；
- operation result不是durable truth，只有对应entry append/apply后才改变projections。

## SessionExecutor主循环

实现可以使用`tokio::select!`、local executor或等价机制，但interface不要求`Send + tokio::spawn`。

```rust
loop {
    tokio::select! {
        request = request_queue.recv() => {
            handle_session_request(request).await?;
        }

        result = running_operations.next() => {
            handle_operation_result(result).await?;
        }

        deadline = interaction_deadlines.next() => {
            handle_interaction_timeout(deadline).await?;
        }
    }
}
```

实现要求：

- 不能在循环中等待完整Model request、完整Tool execution或用户approval；
- request handler只能更新state、保存response sender或启动operation，然后返回主循环；
- 可以等待一次短SessionWriter append取得确定结果；
- 如果文件adapter使用blocking syscall，SessionWriter implementation可以在内部offload I/O，但不能产生第二个Session semantic owner；
- request和operation result的状态修改只发生在SessionExecutor；
- progress event不进入request queue。

## AgentLoop Interface

AgentLoop是crate-private concrete implementation或private adapter，不定义public trait。

```rust
pub(crate) enum AgentLoopAction {
    NeedModel {
        output_contract: Option<OutputContract>,
    },
    NeedTools {
        response: FinalizedAssistantResponse,
        calls: Arc<[ToolCall]>,
    },
    Finished {
        response: FinalizedAssistantResponse,
    },
}

impl AgentLoop {
    pub(crate) fn next_action(&mut self) -> Result<AgentLoopAction, AgentLoopError>;

    pub(crate) fn accept_model_response(
        &mut self,
        response: FinalizedAssistantResponse,
    ) -> Result<(), AgentLoopError>;

    pub(crate) fn accept_committed_tool_round(
        &mut self,
        round: CommittedToolRound,
    ) -> Result<(), AgentLoopError>;

    pub(crate) fn accept_committed_steer(
        &mut self,
        change: CommittedConversationDelta,
    ) -> Result<(), AgentLoopError>;
}
```

AgentLoop可以使用Rig或其他SDK作为private adapter，但不得：

- 读取SessionStorage；
- 调用SessionWriter；
- 直接调用PromptService、ToolService或SkillService；
- 读取current Session definition或Workspace；
- 执行Tool；
- 处理approval；
- 发布Runtime event；
- 决定Turn terminal entry；
- 在`tool_round_completed`前把ToolResult加入下一次模型调用。

## Append与Projection更新

SessionExecutor使用一个private helper统一写入：

```rust
async fn append_and_apply(
    &mut self,
    draft: SessionEntryDraft,
) -> Result<CommittedSessionEntry, SessionExecutionError>;
```

行为：

```text
SessionWriter.append(draft)
→ 如果OutcomeUnknown：停止后续动作并reopen/replay或lookup operation key
→ 得到CommittedSessionEntry
→ apply Turn/Item/Interaction/Conversation/Usage/tree deltas
→ 返回receipt
```

只有`append_and_apply`成功后，SessionExecutor才能：

- 发布由该entry产生的Runtime event；
- 通知Interaction request；
- 恢复等待中的Tool；
- 允许Tool副作用；
- 启动下一次Model操作；
- 返回Submit Started；
- 返回ResolveInteraction成功；
- 返回Cancel完成。

projection apply失败时：

- durable entry不回滚；
- 停止当前执行；
- 丢弃hot projections；
- 从SessionStorage replay；
- replay成功后按durable truth继续或terminalize；
- replay失败则Session进入Unavailable。

## Submit流程

普通Submit只在`Idle + Open + Loaded + Ready + accepting_requests`时接受。

```text
Submit
→ 验证Session lifecycle/load/readiness
→ state = Starting
→ 创建SubmissionId、candidate TurnId和execution_version=1
→ 启动BuildTurnContext
→ Context result identity/version validation
→ 启动ComposeUserMessage(source=Input)
→ composition result identity/version validation
→ 与Agent status update串行化
→ final AgentStatus = Enabled check
→ 取得WorkspaceCommitAuthorization
→ append/apply TurnContext
→ append/apply UserMessage(source=Input)
→ release WorkspaceCommitAuthorization
→ 释放Agent status synchronization
→ 创建CurrentTurnExecution和AgentLoop
→ state = Running
→ 返回Started { turn_id }
→ drive_agent_loop
```

规则：

- Starting时第二个Submit返回`SessionBusy`；
- Submit request handler保存response sender、启动BuildTurnContext后立即返回主循环；
- Context append成功、UserMessage append失败时，Context是安全orphan，不创建领域Turn；
- UserMessage append OutcomeUnknown必须先按operation key解析，不能创建第二个TurnId；
- UserMessage append/apply前不调用Model或Tool；
- Agent disable先完成则Submit失败；UserMessage append先完成则active Turn继续；
- TurnExecutionContext capture失败不创建Failed Turn；
- cancellation在UserMessage append前获胜时回到Idle。

`Context.compose_message(...)`可能lazy-load Skill，因此必须作为异步operation执行，不能在SessionExecutor request handler中直接等待。

## Drive AgentLoop

```rust
async fn drive_agent_loop(&mut self) -> Result<(), SessionExecutionError> {
    loop {
        match self.current_turn_mut()?.agent_loop.next_action()? {
            AgentLoopAction::NeedModel { .. } => {
                self.start_model_operation().await?;
                return Ok(());
            }
            AgentLoopAction::NeedTools { response, calls } => {
                self.start_tool_operation(response, calls).await?;
                return Ok(());
            }
            AgentLoopAction::Finished { response } => {
                self.finish_completed_turn(response).await?;
                return Ok(());
            }
        }
    }
}
```

`drive_agent_loop`只推进到下一个异步操作或terminal处理，不在一个调用中等待完整Turn。

## Model流程

```text
AgentLoop NeedModel
→ 从projections取得CommittedConversationView
→ TurnExecutionContext.assemble_model_context(AgentRun)
   ├─ success：保存immutable AssembledModelContext和ModelCallRequest，再检查soft pressure
   └─ PromptContextOverflow：直接进入hard Compaction planning，没有valid ModelCallRequest
→ no pressure：phase = Sampling
   → GenerateModelResponse调用ModelGateway(AgentRun request)
→ soft/hard pressure：phase = Compacting
   → CompactConversation调用PromptSet和ModelGateway(CompactionSummary request)
→ Executor继续处理SessionRequestQueue
→ OperationResult返回并校验SessionId/TurnId/execution_version/OperationType
   ├─ Model：AgentLoop.accept_model_response → drive_agent_loop
   └─ Compaction：revalidate → append/apply Replace → rebuild AgentLoop → reassemble
```

规则：

- PromptSet是唯一context assembly实现；
- ModelGateway只接收validated `ModelCallRequest`；该request的唯一model-visible input是`AssembledModelContext`；
- provider retry复用同一个immutable ModelCallRequest；
- provider transport fallback不得改变exact provider/model identity；
- active Turn内不允许transparent cross-model fallback；
- logical retry只允许在ConversationCheckpoint、TurnExecutionContext、purpose、output contract和effective max_output_tokens均未改变时发生；
- streaming delta通过ProgressEventPublisher发布，不写SessionStorage；
- partial response不是AgentMessage Item；
- Model result只有在AgentLoop返回NeedTools或Finished后才决定对应entry类型；
- NeedTools保存Assistant(Intermediate)；Finished在Steer/Cancel处理后保存Assistant(Final)。

## Tool流程

```text
AgentLoop NeedTools { finalized response, calls }
→ 处理已进入SessionRequestQueue的Cancel/Steer/WorkspaceAuthorizationRevoked
→ 重新检查Workspace authorization lease
→ Steer获胜：丢弃尚未持久化的model output，进入Steer composition
→ Cancel/revocation获胜：不保存Assistant(Intermediate)，进入Interrupted处理
→ Turn仍Running：取得WorkspaceCommitAuthorization
→ append/apply Assistant(Intermediate)
→ release WorkspaceCommitAuthorization
→ ToolInvocation projection = Started
→ phase = ExecutingTools
→ 启动ToolSet.execute；需要approval时phase = WaitingApproval
```

ToolSet流程：

```text
preflight / schema / hook / policy
→ optional ToolExecutionControl.request_approval
→ ToolExecutionControl.record_execution_start
→ sandbox / executor
→ truthful ToolExecutionOutcome
```

Tool结果处理：

```text
ToolExecutionOutcome[]
→ 每个Completed outcome append/apply role=tool message
→ 任一Abandoned：append/apply ToolAbandoned并进入Turn terminal处理
→ 全部Completed：处理已经进入SessionRequestQueue的Cancel和WorkspaceAuthorizationRevoked
→ Cancel/revocation已生效：保留tool messages，不append tool_round_completed，完成Interrupted处理
→ Turn仍Running：取得WorkspaceCommitAuthorization
→ append/apply tool_round_completed
→ release WorkspaceCommitAuthorization
→ conversation projection加入assistant/tool sequence
→ AgentLoop.accept_committed_tool_round
→ 先处理pending Steer
→ drive_agent_loop
```

规则：

- Assistant(Intermediate)一旦append/apply，当前Tool round必须先得到Completed或Interrupted处理；此后到达的Steer只能排队，不能插入assistant/tool sequence；
- ToolSet按source call order返回outcomes；
- Tool内部允许并发无冲突调用；
- Tool message append OutcomeUnknown只解析storage write，不能重新执行Tool；
- exact ToolResult必须保存，即使Cancel已经发生；
- outcome unknown不能生成Tool message；
- Cancel/security revocation在tool messages之后、`tool_round_completed`之前生效时，truthful results保持durable但不进入conversation；
- `tool_round_completed`必须exact cover assistant中的全部ToolCall；
- 下一次Model操作必须等待该entry append/apply；
- Tool result progress不是durable Item。

## Interaction流程

Approval request：

```text
ToolExecutionControl.request_approval
→ SessionExecutor验证Turn/Item/version
→ append/apply InteractionRequested
→ 注册Pending Interaction和deadline
→ 保存Tool control response sender
→ EventPublisher通知host
→ request handler返回主循环
→ Tool future等待typed decision
```

Resolution：

```text
ResolveInteraction(expected TurnId, RequestId, resolution_key)
→ 验证family、deadline和Pending state
→ append/apply InteractionResolved
→ 删除deadline
→ 返回typed decision给Tool future
→ request response成功
```

Timeout：

```text
now >= expires_at
→ SessionExecutor处理deadline
→ append/apply InteractionResolved(Expired或fail-closed resolution)
→ 恢复Tool future
→ late host response返回InteractionExpired或AlreadyResolved
```

规则：

- request notification允许at-least-once delivery；
- reconnect使用相同RequestId；
- same resolution_key重试幂等；
- different key在Resolved后返回AlreadyResolved；
- disconnect不自动关闭Interaction；
- no-interaction host也必须写入明确fail-closed resolution；
- WaitingApproval时TurnStatus仍为Running；
- request_approval handler不能等待host response；ResolveInteraction或timeout负责完成保存的Tool control response sender。

## Steer流程

Steer属于current Turn，必须携带`expected_turn_id`。

### PreparingModel

如果尚未启动Model操作：

```text
启动ComposeUserMessage(source=Steer)
→ composition result identity/version validation
→ 取得WorkspaceCommitAuthorization
→ append/apply UserMessage(source=Steer)
→ release WorkspaceCommitAuthorization
→ AgentLoop.accept_committed_steer
→ 使用更新conversation启动Model
→ SteerResult::Applied
```

### Sampling

```text
execution_version += 1
→ best-effort cancel当前Model操作
→ 启动ComposeUserMessage(source=Steer)
→ composition result identity/version validation
→ 取得WorkspaceCommitAuthorization
→ append/apply UserMessage(source=Steer)
→ release WorkspaceCommitAuthorization
→ AgentLoop.accept_committed_steer
→ 旧version Model result不再使用
→ 启动新的Model操作
→ SteerResult::Applied
```

### Compacting

Compaction期间Steer进入pending Steer FIFO，不立即append，也不改变Compaction source。Compaction append/apply成功或soft fallback结束后，再按FIFO compose/append Steer并从新conversation开始AgentRun assembly。Cancel和Workspace revocation按control FIFO取消Compaction并推进execution version。

### WaitingApproval或ExecutingTools

```text
验证expected TurnId
→ 将PromptIntent放入bounded pending Steer FIFO
→ SteerResult::Queued
→ 当前Tool round得到truthful结果
→ tool_round_completed append/apply
→ 按FIFO逐条启动ComposeUserMessage
→ 每条composition result校验后取得WorkspaceCommitAuthorization并append/apply Steer
→ AgentLoop.accept_committed_steer
→ 下一次Model操作
```

规则：

- queued Steer在append前不是durable fact；
- `SteerResult::Queued`包含process-local completion handle；Steer最终Applied或Rejected时完成该handle；
- queue满返回`SteerQueueFull`；
- process crash会丢失尚未append的queued Steer；
- MVP不使用Steer隐式取消approval；
- MVP不因为Steer取消已经可能产生副作用的Tool；
- Steer不创建新Turn，不capture新TurnExecutionContext；
- Steer composition失败时返回Rejected；Sampling已被取消时使用原conversation重新启动Model；
- final assistant尚未append时，已进入request queue的Steer先处理则继续Turn；
- final assistant append后到达的Steer返回ExpectedTurnMismatch或TurnNotRunning。

## FollowUp流程

FollowUp不是current Turn control。

```text
FollowUp request
→ 放入bounded process-local FIFO
→ FollowUpResult::Queued { follow_up_id, completion }
→ current Turn进入Finishing
→ terminal entry append/apply
→ release CurrentTurnExecution
→ state = Idle
→ 取出下一条FollowUp
→ 作为普通Submit进入Starting
→ capture新的TurnExecutionContext
```

规则：

- FIFO保留accepted ordering；
- PendingFollowUp保存process-local completion sender；下一Turn的UserMessage append/apply后返回Started，admission失败时返回Rejected；
- queue满返回`FollowUpQueueFull`，不删除旧请求；
- FollowUp不持久化，restart后不恢复；
- FollowUp不复用旧TurnId、Context、ToolSet、PromptSet、SkillCatalog或Workspace lease；
- previous Turn Completed、Interrupted或Failed后都可以重新admit FollowUp；
- 如果Session变为Archived、Deleted、Unavailable或Agent Disabled，FollowUp admission失败并通过completion handle返回typed Rejected result；
- PrepareForUnload拒绝新的FollowUp并明确结束尚未执行的queued requests；
- crash-safe FollowUp acknowledgement需要未来storage schema，当前不提供。

## Completed Turn流程

```text
AgentLoop Finished { finalized response }
→ 停止新的Model/Tool操作
→ 处理已经进入request queue的Steer、Cancel和WorkspaceAuthorizationRevoked
→ 重新检查Workspace authorization lease
→ Steer先处理：不append final，继续Running
→ Cancel/revocation先处理：进入Interrupted流程
→ request queue无相关请求且lease有效：取得WorkspaceCommitAuthorization
→ state = Finishing
→ 验证无Pending Interaction、Started ToolInvocation或未完成Intermediate response
→ append/apply Assistant(Final)
→ release WorkspaceCommitAuthorization
→ drop AgentLoop和TurnExecutionContext
→ state = Idle
→ 启动下一条FollowUp或保持Idle
```

Assistant(Final) append是Completed Turn唯一结束线性化点。

## Cancel流程

```text
Cancel(CancelTarget::Turn(expected TurnId))
→ 验证current Running Turn
→ execution_version += 1
→ state = Finishing
→ best-effort cancelContext/Model和可取消Tool操作
→ resolve/cancel Pending Interaction
→ 对尚未执行的Started invocation生成truthful Cancelled ToolResult并append/apply Tool message
→ 对已记录ToolExecutionStarted的调用等待、取消或确认outcome
→ exact result append/apply Tool message
→ only outcome unknown append/apply ToolAbandoned
→ append/apply TurnInterrupted
→ drop AgentLoop和TurnExecutionContext
→ state = Idle
→ 处理FollowUp或保持Idle
```

规则：

- Cancel不回滚已经发生的外部副作用；
- Cancel不删除已经append的User/Assistant/Tool entry；
- Cancel不能生成synthetic ToolResult；尚未执行且已确认取消可以生成truthful Cancelled ToolResult；
- Model partial response直接丢弃；
- Cancel response只在TurnInterrupted append/apply完成后返回；
- duplicate Cancel在相同terminal fact后幂等返回当前结果；
- Cancel与Assistant(Final)由Executor实际处理顺序决定；
- Final先append则Cancel返回TurnNotRunning；Cancel先处理则Final candidate丢弃。

Cancel request handler不等待Tool outcome或terminal append。它保存response sender、更新state和cancellation token后返回主循环；后续OperationResult或timeout处理继续cleanup，TurnInterrupted append/apply后完成Cancel response。

Starting期间使用`CancelTarget::Submission(submission_id)`：

```text
cancel BuildTurnContext / ComposeUserMessage
→ discard candidate Context/message
→ complete Submit response as Rejected(Cancelled)
→ complete Cancel response
→ state = Idle
```

## Workspace Authorization Revocation

Workspace-dependent conversation entry使用private `WorkspaceCommitAuthorization`消除authorization check与append之间的竞态。覆盖initiating TurnContext/UserMessage、Steer UserMessage、Model-produced Assistant、`tool_round_completed`和active-Turn `StoredCompaction`：

```text
WorkspaceAuthorizationLease.authorize_commit()
→ 与WorkspaceAuthorizationControl.revoke()使用同一同步原语排序
→ revocation先线性化：authorize_commit失败，不允许append
→ authorize_commit先线性化：短暂持有authorization，完成append/apply后释放
→ revoke等待已有authorization释放，然后标记lease revoked
```

`WorkspaceCommitAuthorization`只跨越一次短SessionWriter append/apply sequence，不能跨Model/Tool I/O、request handler等待或完整Turn。它不替代entry append线性化点；它保证revocation不可能在authorization validation和对应append之间生效。

WorkspaceAuthorizationControl撤销当前lease后，把`WorkspaceAuthorizationRevoked`写入SessionRequestQueue。

```text
WorkspaceAuthorizationRevoked
→ 重新检查current Turn使用的lease已经revoked
→ execution_version += 1
→ state = Finishing
→ cancel Context/Model和可取消Tool operation
→ 已开始副作用的Tool按Cancel规则确认outcome
→ 不append缺失的tool_round_completed
→ append/apply TurnInterrupted(SecurityRevoked)
→ release current Turn
→ state = Idle或Unavailable
```

规则：

- Model启动和ToolExecutionStarted记录前重新检查lease；workspace-dependent conversation entry必须在WorkspaceCommitAuthorization内append/apply；
- 即使revocation request尚在queue中，已revoked lease也必须使authorization validation失败；
- revocation不能撤回provider已经看到的内容，也不能回滚已发生副作用；
- 已append但conversation-hidden的assistant/tool entries保持durable；
- Starting期间revocation取消candidate submission，不创建领域Turn；
- FollowUp重新admit时使用current readiness和新的WorkspaceSnapshot。

## Failure流程

### Starting failure

Context capture、compose、Agent status validation或initiating append失败：

```text
cancel candidate operations
→ drop candidate Context/drafts
→ state = Idle
→ return Rejected
```

Context entry已经append但UserMessage未append时，保留orphan Context，不创建Failed Turn。

### Running failure

不可恢复Model、Tool、storage或invariant failure：

```text
state = Finishing
→ close Pending Interaction
→ settle Started ToolInvocation
→ append/apply TurnFailed
→ releasecurrent Turn
→ state = Idle
```

Tool自己的failed/denied result通常是truthful ToolResult，不自动使Turn Failed。

### Projection failure

见Append与Projection更新：先replay durable truth，无法恢复时Session Unavailable。

## Retry

### Provider retry

由ModelGateway内部处理连接、认证刷新、same-model attempt和transport fallback。必须复用同一个immutable ModelCallRequest；active Turn内不得替换provider/model identity。

### Logical model retry

由SessionExecutor决定：

```text
Model operation retryable failure
→ 确认TurnId/execution_version未变
→ 确认ConversationCheckpoint未变
→ 确认TurnExecutionContext未变
→ 使用相同ModelCallRequest重试
→ increment logical retry_count
```

Retry delay使用timer，不阻塞SessionRequestQueue。

Steer、Cancel、Compaction或任何model-visible conversation change都会使旧logical retry失效。`RequestOutcomeUnknown`和`StreamInterrupted`的logical retry可能重复provider work或billing；Session policy必须显式限制次数并保留diagnostic，不能宣称exactly-once。

### Tool retry

- pre-execution validation/policy错误可以重新构造新的调用结果，但不自动重新执行同一个side effect；
- ToolExecutionStarted后outcome unknown禁止自动retry；
- SessionWrite NotCommitted/OutcomeUnknown只允许重试/解析同一entry write，不允许重放Tool。

## Context Overflow与Compaction连接

具体cut、summary和storage规则见[Compaction架构设计](compaction.md)。SessionExecutor固定以下调用关系：

```text
AgentLoop NeedModel
→ assemble AgentRun context并检查soft pressure/local overflow
→ 必要时Compaction.plan
→ TurnExecutionPhase = Compacting
→ RunningOperation::CompactConversation
→ 验证Turn/version/source/authorization/control state
→ append/apply StoredCompaction
→ conversation projection Replace
→ rebuild ConversationSeed和private AgentLoop segment
→ 使用同一个TurnExecutionContext继续Model操作
```

provider ContextOverflow也回到同一流程；同一Turn最多一次automatic overflow recovery。

规则：

- Compaction不替换TurnExecutionContext、TurnModelSnapshot或PromptSet；
- 启动Compaction不推进execution_version；成功Replace后推进，Cancel/revocation仍立即推进；
- active Turn initiating UserMessage及其后的连续suffix必须原样保留；
- `Compacting`期间Steer排队，Cancel和Workspace revocation可以取消operation并在append前获胜；
- Compaction result append前必须重新验证source checkpoint和TranscriptFingerprint；
- soft-pressure失败只有在原ModelCallRequest仍exact valid时才能继续；
- hard overflow失败或compact后仍overflow时TurnFailed；
- success后必须重建AgentLoop segment，不能resume携带旧history的run；
- summary operation、plan和retry timer不跨restart恢复。

## PrepareForUnload

`PrepareForUnload`用于graceful shutdown或显式等待Session停止执行：

```text
accepting_requests = false
→ reject new Submit/Steer/FollowUp
→ 明确结束queued Steer/FollowUp requests
→ 如果Idle：立即返回
→ 如果Starting/Running/Finishing：保存PrepareForUnload response sender并立即返回主循环
→ 继续处理ResolveInteraction、Cancel、operation result和timeout
→ terminal后完成所有PrepareForUnload responses
→ Runtime移除SessionExecutionHandle并释放Executor
```

规则：

- PrepareForUnload request handler不能等待current Turn完成；
- PrepareForUnload不自动Cancel current Turn；
- 需要立即停止时调用方先Cancel；
- ResolveInteraction、Cancel和GetSnapshot在等待期间仍允许；
- public `Unload`是否选择等待或返回Busy由Runtime protocol决定；
- executor释放前writer必须closed，progress publisher停止，所有request response完成。

## Snapshot

`GetSnapshot`从Executor当前state和projections构造：

```text
Session lifecycle/readiness
SessionExecutionState
current TurnId/TurnStatus/phase
committed Items
Pending Interactions
queued Steer/FollowUp count
running OperationType
latest usage
```

Snapshot不包含：

- provider client或raw response；
- Tool executor handle；
- Prompt/Skill正文；
- credentials；
- mutable references；
- streaming partial buffer作为authoritative content。

Snapshot请求经过SessionRequestQueue，因此观察到之前已经处理的请求和operation results。

## Progress Event

`ProgressEventPublisher`处理高频、非durable事件：

```text
assistant text/reasoning delta
Tool stdout/progress
provider retry notification
phase change notification
```

规则：

- 使用独立bounded queue；
- 可以按SessionId/TurnId/ItemId合并连续delta；
- queue满时允许丢弃中间progress，但不能丢失durable final event；
- final event从append/apply后的entry生成，包含完整final snapshot；
- progress publisher失败不影响SessionStorage或Turn terminal；
- reliable状态变化进入per-session StateEvent并推进SessionCursor；ProgressEvent不占用cursor且可以合并/丢弃。完整reconnect规则见[Runtime Interface](runtime-interface.md)。

## Request与Operation处理顺序

SessionExecutor是唯一状态修改者。线性化点：

| 操作 | 线性化点 |
| --- | --- |
| Submit开始Turn | initiating UserMessage append |
| Steer应用 | Steer UserMessage append |
| Interaction request | InteractionRequested append |
| Interaction resolution | InteractionResolved append |
| Tool允许副作用 | ToolExecutionStarted append |
| Tool result确定 | Tool message append |
| Tool round进入conversation | tool_round_completed append |
| Workspace authorization revocation | WorkspaceAuthorizationControl.revoke |
| Turn Completed | Assistant(Final) append |
| Turn Interrupted | TurnInterrupted append |
| Turn Failed | TurnFailed append |

并发规则：

- request queue中的两个mutation按接收顺序处理；
- operation result与request同时ready时，由Executor处理顺序决定；
- ToolControl、Cancel和WorkspaceAuthorizationRevoked使用同一个FIFO；处理ToolExecutionStarted时，所有排在它之前的request已经完成；
- Tool results全部保存后、append tool_round_completed前，处理当前已经进入queue的Cancel和WorkspaceAuthorizationRevoked；
- 处理Finished action前，处理当前已经进入queue的Steer、Cancel和WorkspaceAuthorizationRevoked，并重新检查authorization lease；
- 上述queue处理只读取当前已排队request，不等待未来request；
- workspace-dependent conversation entry的append必须持有WorkspaceCommitAuthorization；它与WorkspaceAuthorizationControl.revoke的先后顺序决定append还是revocation获胜；
- 不涉及Workspace authorization的其他append在queue检查完成后可以继续，并成为该race的线性化结果；
- 每个处理函数结束时state和projections必须保持合法；
- 任何异步operation不能直接修改SessionExecutor字段。

## Multi-Session并发

一个Runtime中的多个Executor独立推进：

```text
Session A: Sampling
Session B: WaitingApproval
Session C: ExecutingTools
Session D: Idle
```

必须保证：

- Runtime request按SessionId路由；
- 每个SessionStorage文件只有对应Executor的writer；
- shared services实现并发安全；
- ModelGateway提供global/provider/model/auth-principal并发限制；
- ToolService资源锁使用canonical resource identity，不使用SessionId代替物理资源identity；
- 同一Workspace中的冲突write Tool跨Session串行；
- read-only无冲突Tool可以跨Session并行；
- 一个Session的request/progress queue达到容量不能阻塞其他Session；
- Runtime shutdown逐Session执行Cancel/PrepareForUnload；
- UI current tab不影响后台Session执行。

## Error分类

```rust
pub enum SessionExecutionError {
    SessionNotOpen,
    SessionNotLoaded,
    SessionNotReady,
    SessionUnavailable,
    ExecutorStopping,
    SessionBusy,
    RequestQueueFull,
    SteerQueueFull,
    FollowUpQueueFull,
    SubmissionNotFound,
    NoRunningTurn,
    ExpectedTurnMismatch,
    WorkspaceAuthorizationRevoked,
    InteractionNotFound,
    InteractionAlreadyResolved,
    InteractionExpired,
    ToolControlRejected,
    StaleOperationResult,
    Storage(SessionWriteError),
    Projection(ProjectionError),
    Context(TurnContextError),
    AgentLoop(AgentLoopError),
    Model(ModelCallError),
    Tool(ToolExecutionError),
    InvariantViolation,
}
```

分类原则：

- expected identity/version mismatch不是storage corruption；
- stale operation result通常记录diagnostic后忽略，不一定返回给host；
- storage OutcomeUnknown必须解析，不能转换成Tool outcome unknown；
- Tool failed result与Tool execution infrastructure error分开；
- QueueFull是明确backpressure，不重试/丢弃已有请求；
- invariant violation停止当前Session执行并触发replay或Unavailable。

## Recovery

SessionExecutor只在load/recovery完成后对外可用。

```text
open SessionStorage
→ replay entries和projections
→ 若无Running Turn：构造Idle Executor
→ 若有unfinished Running Turn：
     append/apply pending Interaction closure
     preserve existing Tool messages
     append/apply ToolAbandoned for remaining Started invocation
     append/apply TurnInterrupted
→ 确认projection closed
→ 构造Idle Executor
→ publish loaded Session handle
```

不恢复：

```text
old SessionRequestQueue contents
queued Steer
FollowUp queue
AgentLoop internal state
Context/UserMessage composition/Model/Tool/Compaction async operation
approval waiter
provider continuation/session
Workspace authorization lease
```

如果terminal entry已存在但projection仍有Pending Interaction或Started Item，load fail closed并要求explicit repair。

## Performance

MVP性能要求：

- 一个loaded Session一个Executor；
- SessionRequestQueue、Steer queue和FollowUp queue都有容量限制；
- 每个execution version最多一个current RunningOperation；旧version operation只等待取消或迟到result；
- ToolSet内部负责ToolCall并发；
- SessionWriter复用open file handle和buffer；
- 短entry append可以由Executorawait；blocking I/O由SessionWriter内部实现处理；
- streaming progress不写JSONL；
- progress event可以合并；
- snapshot从hot projections构造，不重放完整文件；
- Model/Tool并发配额由共享模块统一执行，不在每个Executor复制全局计数；
- bounded queue达到容量时返回明确错误，不能无限增长。

## Test Matrix

### State与Admission

- Idle Submit进入Starting；
- Starting期间第二个Submit返回SessionBusy；
- CancelTarget::Submission在Context capture和UserMessage composition期间取消candidate；
- Session不是Open/Loaded/Ready时拒绝Submit；
- Context failure不创建Turn；
- stale BuildTurnContext/ComposeUserMessage result在version变化后被忽略；
- Context entry成功、UserMessage失败产生安全orphan；
- Agent disable与UserMessage append的最终顺序；
- UserMessage OutcomeUnknown按operation key恢复；
- UserMessage append前不调用Model/Tool。

### Model

- NeedModel只使用CommittedConversationView；
- Model运行期间处理Steer、Cancel、Snapshot；
- provider retry复用相同ModelCallRequest且不改变model identity；
- request delivery outcome unknown和first semantic delta后的stream failure不由Gateway blind retry；
- execution version过期的Model result被忽略；
- Finished action在Steer/Cancel/revocation处理和commit authorization后才append final；
- NeedTools result处理前FIFO已有Steer时丢弃未持久化model output，不创建未闭合Tool round；
- streaming delta丢失不影响final entry；
- transport fallback保持exact model identity；
- cross-model substitution返回typed failure而不是静默继续；
- logical retry_count与Gateway transparent retry count分离。

### Tool

- Assistant(Intermediate)先于Tool执行；
- Cancel在ToolExecutionStarted前获胜时不执行副作用；
- Cancel在SessionRequestQueue中排在ToolControl request之前时，record_execution_start被拒绝；
- ToolExecutionStarted先获胜时Cancel不声称回滚；
- approval request append-before-notify；
- resolution append-before-resume；
- timeout与host resolution first-wins；
- parallel ToolCall按source order保存results；
- 每个ToolResult独立append；
- partial results不进入conversation；
- tool_round_completed后完整round一次进入conversation；
- revocation在tool messages后、tool_round_completed前到达时不把round加入conversation；
- Tool message append OutcomeUnknown不重放Tool；
- side-effect outcome unknown产生ToolAbandoned。

### Steer与FollowUp

- PreparingModel Steer直接应用；
- Sampling Steer推进version并取消旧Model；
- old Model result被忽略；
- Compacting/WaitingApproval/ExecutingTools Steer进入FIFO；
- queued Steer在tool_round_completed后按序append；
- Steer queue满返回错误；
- final append与Steer first-processed-wins；
- FollowUp在current Turn terminal后创建新Turn和新Context；
- FollowUp FIFO和QueueFull；
- restart不恢复FollowUp；
- FollowUp后续admission失败通过completion handle返回Rejected；
- PrepareForUnload明确结束queued requests；
- PrepareForUnload等待期间仍能处理ResolveInteraction、Cancel、operation result和timeout。

### Cancel、Security Revocation与Failure

- Cancel during Context capture；
- Cancel during Sampling；
- Cancel during Compaction；
- Cancel during WaitingApproval；
- Cancel during Tool execution before/afterexecution-start record；
- Cancel保存exact ToolResult；
- Cancel只生成一个TurnInterrupted；
- duplicate Cancel幂等；
- WorkspaceAuthorizationRevoked during Starting/Compacting/Sampling/WaitingApproval/ExecutingTools；
- revoked lease使Model start、record_execution_start和tool_round_completed validation失败；
- WorkspaceCommitAuthorization与revoke双向race：revoke先赢时不append，authorization先赢时完成短append后再中断；
- Running failure关闭Pending/Started state；
- projection apply failure触发replay；
- replay failure使Session Unavailable。

### Multi-Session

- 两个Session同时Sampling；
- 一个Session WaitingApproval不阻塞另一个Session；
- 同一物理文件write Tool跨Session串行；
- 不同资源Tool跨Session并行；
- 一个Session request queue满不影响其他Session；
- UI selection变化不影响后台Session；
- shutdown逐Session停止且不串写entry/event。

### Recovery

- restart清除所有async operations和queues；
- unfinished Turn逐entryterminalize；
- existing Tool message保留；
- 不补写tool_round_completed；
- outcome unknown Tool不自动执行；
- recovery operation key保证重复load幂等；
- terminal entry后Pending/Started被判定为corruption。

## Diagnostics

至少记录：

```text
SessionId / TurnId / ItemId / RequestId
SessionExecutionState / TurnExecutionPhase
execution_version
OperationType和duration
request queue / Steer queue / FollowUp queue depth
Model/Tool cancellation result
SessionWriter error class
projection replay count
late operation result count
Tool outcome unknown count
```

默认不记录：

```text
credentials
raw provider payload
full Prompt/Skill content
secret Interaction answer
unredacted Tool arguments
hidden chain-of-thought
```

## 明确不建立

```text
SessionManager领域对象
TurnManager
ItemManager
InteractionService
ModelStep entity
ToolRound entity
public AgentLoop trait
每个子职责一个Executor
第二Session writer
第二conversation projection
unbounded request/progress queue
全局current Session
恢复旧provider stream或Tool task
```

## 被否决的方案

### SessionExecutor直接等待完整Turn

否决原因：Model、Tool或approval期间无法及时处理Steer、Cancel、ResolveInteraction和Snapshot。

### Arc<Mutex<SessionExecutor>>

否决原因：跨await持锁会产生死锁；不跨await则状态修改分散到多个调用方，无法保证entry和projection顺序。

### 强制two-owner execution

否决原因：容易让独立operation task拥有AgentLoop、writer、queue或terminal状态，形成第二owner。只有Rig adapter确实只能以monolithic future运行时，才允许private adapter task；它只返回OperationResult。

### 每个Model/Tool/Interaction一个actor

否决原因：增加queue、shutdown、ordering和测试复杂度，但MiniCore每Session同时只有一个active Turn，没有对应收益。

### AgentLoop直接写SessionStorage

否决原因：Prompt、Tool、Interaction和Turn terminal的顺序会散落在SDK adapter内部，无法独立测试或替换AgentLoop。

### 一个Runtime只允许一个Running Session

否决原因：UI需要后台Session；SessionStorage、TurnExecutionContext和request queue已经按Session隔离，共享资源可以用明确配额和resource locks协调。

## 完成检查

- [x] 确定一个loaded Session一个SessionExecutor。
- [x] 确定一个Runtime允许多个Running Session。
- [x] 定义SessionRequestQueue和request response语义。
- [x] 定义SessionExecutionState和CurrentTurnExecution。
- [x] 定义RunningOperation和operation identity/version校验。
- [x] 定义private AgentLoop interface。
- [x] 定义Submit、Model、Tool和terminal主流程。
- [x] 定义ToolExecutionControl和Interaction顺序。
- [x] 定义Steer sampling cancellation和Tool阶段queue语义。
- [x] 定义FollowUp bounded process-local FIFO。
- [x] 定义Cancel、timeout、retry和failure行为。
- [x] 定义PrepareForUnload和Snapshot。
- [x] 定义recovery和multi-session并发规则。
- [x] 定义performance和测试矩阵。
- [ ] 执行Rig 0.40.0 adapter spike。
- [ ] 实现SessionExecutor和自动化测试。
- [x] 在阶段9冻结公开Runtime protocol。
