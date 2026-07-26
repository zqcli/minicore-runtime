# Session Execution 架构设计

日期：2026-07-25

状态：当前权威架构（设计已冻结，生产实现待启动）

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
- [Tool子系统架构设计](tools.md)
- [ModelGateway架构设计](model-gateway.md)
- [Compaction架构设计](compaction.md)
- [Agent与Session生命周期架构设计](agent-session-lifecycle.md)
- [Runtime Interface与公开协议架构设计](runtime-interface.md)

## 决策摘要

核心决策：

- 每个loaded Session拥有一个`SessionExecutor`；
- 一个MiniCoreRuntime可以同时运行多个`SessionExecutor`；
- 每个Session同时最多一个Starting或Running Turn；
- `SessionExecutor`是该Session执行期mutable state的唯一owner；
- 外部调用方只持有可克隆的`SessionExecutionHandle`，不能直接借用Executor状态；
- 外部请求进入每个 Session 独立的 `SessionIngress`；Submit、Steer、FollowUp、Interaction、Tool control、emergency 和 lifecycle 使用语义不同的有界 lane，不承诺跨 lane 全局 FIFO；
- Context构造、UserMessage composition、Model调用和Tool执行使用异步`RunningOperation`；Executor继续处理请求，但同一Session不并行启动第二个logical operation；
- 每个异步结果携带`SessionId + TurnId + execution_version + OperationType`；
- logical retry只能在旧operation terminal并从`current_operation`移除后启动；不允许detached本地future继续向Executor返回结果；
- Steer使用普通FIFO并等待当前assistant/tool step完整结束，不取消Sampling，也不丢弃已完成step；
- Tool副作用已经开始时，迟到结果仍必须确认并保存，不能因为version变化而丢弃；
- Session execution驱动private AgentLoop；AgentLoop不拥有storage、Prompt assembly、Tool execution或Turn状态；
- SessionWriter append由Executor发起，receipt必须立即应用到全部required projections；
- initiating UserMessage append/apply后才允许第一次Model调用；
- assistant intermediate和tool messages只有在`tool_round_completed`append/apply后才进入模型conversation；
- Interaction request append/apply后才通知host；resolution append/apply后才恢复Tool执行；UserQuestion等待使用`WaitingForUserInput`且UI只负责presentation；
- `tool_execution_started`append/apply后才允许外部副作用；
- FollowUp使用process-local bounded FIFO，当前不承诺crash-safe delivery；Snapshot使用latest-wins mailbox，不占用mutation/control容量；
- Cancel与Workspace authorization revocation使用sticky、可合并的`EmergencyControl`，不等待普通工作lane的容量；PrepareForUnload使用sticky lifecycle signal和shared completion generation；
- lane容量、拒绝和仲裁规则在`SessionIngress`内定义；一个Session的lane拥塞不能耗尽另一个Session的容量，但共享Model/Tool/I/O资源仍可能形成跨Session等待；
- Progress event可以合并或丢弃，不能影响request处理和durable state；
- restart不恢复旧Context/Model/Tool异步操作，unfinished Turn按既有recovery规则终止。

## 同类项目结论

### pi

pi使用AgentSession包装一个内部AgentLoop：两个薄`PendingMessageQueue`分别保存Steer和FollowUp；默认`one-at-a-time`，在完整assistant+tools turn后取一条Steer，在Agent原本停止时取一条FollowUp。其内外两层循环简单有效。

MiniCore采用Steer/FollowUp分离和内外两层逻辑，但不采用以下部分：

- AgentLoop直接拥有working transcript；
- UserMessage未确认durable就调用模型；
- approval只使用内存hook；
- 没有统一Tool副作用前置记录；
- 没有explicit ToolRoundCompleted conversation规则；
- Abort后没有durable Turn cleanup。
- `all/one-at-a-time`可配置消费模式；MiniCore固定标准FIFO每轮一条，直接使用VecDeque。

### Codex

Codex使用长期Session/Thread状态和active Turn task；active Regular task期间的新输入进入薄`TurnInputQueue/pending_input`，每轮排出并在Tool output后加入ContextManager。它没有pi式独立core FollowUp mode；没有active Turn时输入自然开启新Turn。

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

- 每个Session有独立`SessionIngress`、writer、projections、current Turn和execution version；
- 多个Session可以同时Sampling、WaitingApproval、WaitingForUserInput或ExecutingTools；
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
    ingress: SessionIngress,
    writer: SessionWriter,
    projections: SessionProjections,
    candidate_turn: Option<CandidateTurnExecution>,
    current_turn: Option<CurrentTurnExecution>,
    current_operation: Option<RunningOperation>,
    interaction_deadlines: InteractionDeadlineSet,
    pending_responses: PendingRequestResponses,
    progress_events: ProgressEventPublisher,
}

pub(crate) struct QueuedMessage {
    command_id: CommandId,
    intent: PromptIntent,
}

#[derive(Clone)]
pub(crate) struct SessionExecutionHandle {
    session_id: SessionId,
    ingress: SessionIngressSender,
}
```

`SessionExecutor`不进入领域模型，不持久化，也不通过MiniCoreRuntime公开。

`SessionExecutionHandle`只负责发送请求和接收typed response：

- 不保存Session状态；
- 不提供`&mut SessionExecutor`；
- 不提供通用closure执行；
- 不绕过MiniCoreRuntime的公开权限和路由；
- ingress关闭后返回typed unavailable error。

## SessionIngress 与语义 lane

`SessionIngress`是每个 loaded Session 私有的 ingress boundary，只拥有transport容量和pending input。它不是一个把所有请求混在一起的 FIFO，而是把请求路由到具有独立容量和语义的 lane；Executor仍是唯一的Session state owner，lane只保存尚未处理的输入或控制信号。

```rust
pub(crate) struct SessionIngress {
    pub(crate) turn_control: TurnControlGate,
    pub(crate) turn_admission: BoundedFifo<SubmitRequest>,
    pub(crate) steer: BoundedTurnQueues<QueuedMessage>,
    pub(crate) follow_up: BoundedFifo<QueuedMessage>,
    pub(crate) input_control: InputMailboxControl,
    pub(crate) interaction: BoundedFifo<InteractionControlRequest>,
    pub(crate) tool_control: BoundedFifo<ToolControlRequest>,
    pub(crate) lifecycle: LifecycleControl,
    pub(crate) snapshot: SnapshotMailbox,
}

pub(crate) enum CancelTarget {
    Submission(SubmissionId),
    Turn(TurnId),
}
```

请求与 lane 的完整映射：

| 请求 | lane | 容量与语义 |
| --- | --- | --- |
| `Submit` | `TurnAdmissionQueue` | bounded FIFO；只保存尚未开始的 Turn admission |
| `Steer` | `SteerQueue<TurnId>` | bounded、按目标 Turn 分组的 FIFO；只作用于 current Turn |
| `FollowUp` | `FollowUpQueue` | bounded FIFO；terminal Turn 后作为新 Turn admission |
| `CancelQueuedMessage` | `InputMailboxControl` | 按 `CommandId` 原子删除 Steer 或 FollowUp 中一条，不清空队列 |
| `ResolveInteraction` | `InteractionControlQueue` | bounded FIFO并配置独立保留容量；只处理已存在的 Pending Interaction |
| `ToolControl` | `ToolControlQueue` | bounded FIFO；approval、UserQuestion、execution-start 等内部 durable control |
| `Cancel` | `EmergencyControl` | target-scoped sticky、可合并 signal；每个目标 Submission/Turn 使用共享completion generation，不因普通 lane 满而延迟触发取消 |
| `WorkspaceAuthorizationRevoked` | `EmergencyControl` | sticky signal；先撤销 lease，再唤醒 Executor |
| `PrepareForUnload` | `LifecycleControl` | sticky stop-admission signal + shared completion generation，可附 grace deadline |
| `GetSnapshot` | `SnapshotMailbox` | latest-wins/coalesced 请求，从 immutable published view 读取 |

`SessionIngress`的外层唤醒机制可以是一个很小的 bounded wake channel，但 wake 只表示“某个 lane 有变化”，不承载请求本身；wake 丢失或合并时由 Executor 重新检查各 lane。因此普通工作容量不会成为 emergency、lifecycle 或 snapshot 的隐藏瓶颈。

表中的`EmergencyControl`是`TurnControlGate`暴露的逻辑signal facet，不是另一个queue或owner。active target registration只由Executor在candidate/current Turn切换时发布；发送方只能针对该immutable `CancelTarget + generation`设置signal，不能修改Session state或把stale cancel重定向到新的Turn。

`TurnControlGate`不是queue、actor或第二状态owner，而是`SessionIngress`内部的原子仲裁原语。它只保存active target generation、emergency epoch、Steer admission gate和短commit reservation：

- `try_admit_steer(expected_turn_id)`与`reserve_final_commit(expected_generation)`原子排序。Steer admission先赢时candidate final必须保存为Assistant Continue；final reservation先赢时新Steer返回TurnCancelling/TurnNotRunning；
- `publish_cancel(target, generation)`与`reserve_controlled_append(expected_epoch, kind)`原子排序。Cancel先赢时reservation失败；reservation先赢时该次短append获得胜利，Cancel仍立即设置token并在append后继续cleanup；
- reservation只跨一次短`SessionWriter.append → apply`，不跨Model/Tool I/O、approval或完整Turn；signal发布不阻塞，只记录pending epoch并唤醒Executor；
- 所有以“没有winning emergency”为前置条件的append都必须使用controlled reservation，至少包括initiating UserMessage、Model-produced Assistant、`ToolExecutionStarted`、`tool_round_completed`和active-Turn StoredCompaction；Workspace-dependent append还必须同时取得`WorkspaceCommitAuthorization`。
- append返回`NotCommitted`时释放reservation并先观察pending emergency，再决定是否重新reserve并重试同一draft；`OutcomeUnknown`时保持admission closed、poison writer并按既有保守终结规则处理，不能以旧epoch盲重试。

`LifecycleControl`的stop-admission transition与Submit/Steer/FollowUp的`try_admit`原子排序：input admission先赢时PrepareForUnload必须在drain中明确拒绝/清理它；stop-admission先赢时发送方直接得到`ExecutorStopping`。Emergency、required cleanup control和Snapshot不受该gate阻止。

Ingress gate只能依据Executor发布的immutable target/admission generation做容量预留和race排序；领域validation、StateEvent/cursor推进与typed `Queued` completion仍由Executor确认。这样lane可以独立背压，但不会产生第二个Session read/write owner。

Admission规则：

- lane capacity由Runtime config决定；lane满时返回对应 typed error（如`TurnAdmissionQueueFull`、`SteerQueueFull`、`FollowUpQueueFull`、`InteractionControlQueueFull`或`ToolControlQueueFull`），不静默丢弃已有输入；
- `Submit`成功进入`TurnAdmissionQueue`后只表示等待一次Idle admission决策；它不是隐式FollowUp，也不能跨越另一个新Turn长期等待。未选中且Session已被其他消息占用时及时返回`SessionBusy`；真正的 `Started { turn_id }` 仍在线性化的 initiating UserMessage append/apply 后返回；
- `Steer`/`FollowUp`验证通过并进入各自 FIFO 后立即返回 `Queued`；
- `ResolveInteraction`和`ToolControl`只能对仍然有效的 interaction/turn 生效，过期或不匹配返回 typed rejection；
- `CancelQueuedMessage`只按`CommandId`删除一条仍在 Steer 或 FollowUp lane 的消息；找到即`Cancelled`，找不到统一返回`NotQueued`，不区分从未排队与已经出队；
- `InputMailboxControl.remove(command_id)`与对应lane的`pop_front`使用同一原子同步：remove先赢则消息不执行，pop先赢则返回`NotQueued`且不能把消息重新入队；
- `InputMailboxControl`不直接发布Runtime状态；Executor完成remove、更新immutable snapshot view并发布`queue_updated`后才完成`CancelQueuedMessage` response，因此不产生第二个StateEvent/cursor owner；
- `Cancel`重复请求只按相同`CancelTarget + target generation`合并，调用方订阅同一个completion generation并在同一terminal fact后完成；Executor不为每个duplicate保存独立sender；stale target不得触发当前或下一Turn的cancellation token；
- Emergency signal在目标terminal/取消完成后retire，新的Turn使用新的target generation和CancellationToken；
- revocation signal绑定被撤销的Workspace lease/control generation；future Turn捕获新lease后不继承旧signal；
- `PrepareForUnload`重复请求订阅同一个completion generation，effective grace deadline取现有与新请求的最早值，后续请求不能延长shutdown；
- Snapshot 请求不改变任何 mutation lane 的顺序，不等待普通 queue 排空。

长流程 response 不让 ingress handler 等待完整 operation：

- Submit response 保存在 candidate/admission record 中，UserMessage append/apply 后完成；
- Cancel response等待emergency completion generation，`TurnInterrupted`/其他 terminal fact append/apply 后完成；
- PrepareForUnload response等待lifecycle completion generation，进入 Unloaded 前完成；
- queued Steer/FollowUp 不保存跨崩溃 completion handle；真正 append、新 Turn start 和拒绝由 StateEvent/typed outcome 表达。

## Tool Execution Control

Tool执行使用crate-private `ToolExecutionControl`请求SessionExecutor完成durable操作。权威 interface（包括 `request_approval`、`request_user_question` 和 `record_execution_start`）只在[Tool子系统](tools.md#tool-execution-control)定义；本模块不重复定义 trait，避免两个窄 interface 漂移。

实现把Tool control request写入`ToolControlQueue`。Tool future发送request后等待typed response，但不持有SessionExecutor的锁或mutable reference。`request_approval`与`request_user_question`都由同一SessionExecutor完成InteractionRequested/Resolved的append/apply、deadline与waiter唤醒；前者返回typed approval，后者返回typed UserQuestion answer。

`ToolControlQueue`只保证自身FIFO，不与Steer或Submit建立全局顺序。安全性依赖state-aware arbitration和append线性化点：处理任何`record_execution_start`前，Executor必须先观察`EmergencyControl`的最新epoch并重新验证Turn、cancellation和authorization；若emergency已生效，拒绝新的execution start。若`ToolExecutionStarted`已经append/apply，则该副作用获得真实线性化结果，后续Cancel只能best-effort取消并确认outcome。

SessionExecutor处理Tool control request时必须重新验证：

- expected SessionId和TurnId；
- execution version；
- ItemId/ToolCallId仍属于current Running Turn；
- Turn没有Cancel或security revocation；
- Interaction状态与approval/UserQuestion family匹配；
- Workspace authorization和Tool policy仍允许继续；
- required entry append/apply成功。

Tool control request和外部Cancel同时到达时，由`EmergencyControl`观察与`ToolExecutionStarted` append的先后决定结果：

- Cancel signal先被观察：拒绝新的execution start；
- ToolExecutionStarted先append/apply：副作用可以开始，之后Cancel只能best-effort取消并等待/确认outcome。

## Session Execution State

执行期transient state：

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
- WaitingApproval、WaitingForUserInput、Compacting、Sampling和ExecutingTools是Running Turn的执行阶段，不是Session durable state；
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
    compaction_settings: Arc<CompactionSettingsSnapshot>,
    agent_loop: AgentLoop,
    phase: TurnExecutionPhase,
    cancellation: CancellationToken,
    tool_execution: Option<ToolExecutionState>,
    compaction: Option<CurrentCompactionState>,
    successful_compactions: u32,
    last_hard_compaction_basis: Option<CompactionRecoveryBasis>,
}
```

`CompactionRecoveryBasis`由source checkpoint与当前scope frontier fingerprint组成；active scope的fingerprint同时覆盖effective `previous_checkpoint`和由backing compaction派生的covered-through provenance。同一basis的hard recovery只启动一次；成功StoredCompaction推进checkpoint/frontier后，后续新增completed units可以形成新的basis，但总次数受Turn-pinned `max_compactions_per_turn`限制。

```rust
pub(crate) struct CompactionRecoveryBasis {
    pub source: ConversationCheckpoint,
    pub scope_frontier_fingerprint: CompactionScopeFrontierFingerprint,
}
```

`execution_version`从1开始，在以下情况递增：

- Compaction成功append/apply并Replace conversation；
- Cancel；
- security revocation；
- Turn进入terminal处理。

递增version不能撤销已经append的entry，也不能丢弃已经可能发生副作用的Tool outcome。

## Running Operation

SessionExecutor最多持有一个`current_operation: Option<RunningOperation>`，它是当前逻辑Model/Tool/Compaction工作的唯一execution-local状态。provider attempt和transparent retry完全由ModelGateway内部管理，CurrentTurnExecution不保存并列的ModelAttemptState。operation future由主循环直接poll，不detach成可以在owner不知情时回传结果的后台task。等待Model/Tool I/O时，Executor仍可通过`select!`处理SessionIngress的各 lane；“处理一个 ingress 请求”不等于“并行启动第二个logical operation”。

新operation只能在旧operation满足以下任一条件后启动：

```text
terminal OperationResult已处理并移除
或
对无外部副作用的future执行安全drop，且旧结果通道已关闭
```

Model、Context、composition和未开始副作用的Tool可以在Cancel后安全drop。Tool越过`ToolExecutionStarted`后必须保留为current operation直到exact outcome或明确Abandoned settlement；期间不启动下一次Model/Tool operation。

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
        scope: CompactionScope,
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
- 非current operation产生结果：实现错误；正常实现不存在可迟到的detached Context/Model/Compaction/composition result；
- execution version仍用于验证result基于的Turn control/conversation generation；
- Tool尚未越过`ToolExecutionStarted`记录时，过期结果可以取消/忽略；
- Tool已经越过该记录时，必须确认outcome：exact result保存为Tool message，无法确认则ToolAbandoned；
- terminal Turn不能接受新的Model result、ToolCall或Interaction；
- operation result不是durable truth，只有对应entry append/apply后才改变projections。

## SessionExecutor主循环

实现可以使用`tokio::select!`、local executor或等价机制，但interface不要求`Send + tokio::spawn`。

```rust
loop {
    tokio::select! {
        wake = ingress.wake() => {
            handle_ingress_wakeup(wake).await?;
        }

        result = poll_current_operation(&mut current_operation), if current_operation.is_some() => {
            current_operation = None;
            handle_operation_result(result).await?;
        }

        deadline = interaction_deadlines.next() => {
            handle_interaction_timeout(deadline).await?;
        }
    }
}
```

实现要求：

- 不能在ingress handler中内联等待完整Model request、完整Tool execution或用户approval；主循环通过`select!`同时poll唯一current operation、deadline和lane wakeup；
- wakeup只触发一次state-aware arbitration；handler按lane取出一条或合并signal，然后更新state、保存response sender或启动operation，再返回主循环；
- 可以等待一次短SessionWriter append取得确定结果；
- 如果文件adapter使用blocking syscall，SessionWriter implementation可以在内部offload I/O，但不能产生第二个Session semantic owner；
- ingress和operation result的状态修改只发生在SessionExecutor；
- progress event不进入任何mutation/control lane。

### Lane arbitration

各 lane 没有隐含的全局 FIFO。Executor在每个安全点按当前状态仲裁，固定优先级为：

1. `EmergencyControl` 与 `LifecycleControl`：先应用新的 cancel/revocation epoch，停止 admission；卸载 deadline 到期时 fail-closed；
2. 已完成的 operation result、interaction timeout 和 terminal cleanup；它们负责兑现已经发生的 durable work；
3. `InteractionControlQueue`：只处理当前 Pending Interaction 的 resolution，append/apply 后再恢复 Tool；
4. 当前 Tool round 需要的 `ToolControlQueue` 请求；每次只消费 bounded burst，避免 control flood 持续饿死普通 admission；
5. 当前 Turn 的安全点消费至多一条 `SteerQueue` 消息；
6. Turn terminal 后在 `FollowUpQueue` 与 `TurnAdmissionQueue` 之间做一次公平 admission：已接受的 FollowUp最多获得一次连续优先；若上一Turn由FollowUp启动且当前有external Submit，则先选Submit。未选中的Submit在Session再次Busy时明确返回`SessionBusy`，不能被静默留过整个Turn；
7. `SnapshotMailbox`独立读取最新 immutable published view，不等待上述 lane。

`EmergencyControl`的检查点至少位于启动新 Model、取得 Tool resource lock、`ToolExecutionStarted` append 前，以及 terminal Assistant/`tool_round_completed` append 前。仲裁只处理当前已观察到的 signal，不等待未来请求；因此每个安全点仍有有限、可测试的边界。

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
        change: CommittedConversationDelta,
    ) -> Result<(), AgentLoopError>;

    pub(crate) fn accept_committed_steer(
        &mut self,
        change: CommittedConversationDelta,
    ) -> Result<(), AgentLoopError>;
}
```

`accept_committed_tool_round`的delta只能来自`tool_round_completed`成功append/apply后SessionStorage生成的trusted `CommittedConversationDelta`，不能由execution-local ToolResult自行构造。`Finished`只表示candidate final。Steer FIFO为空时SessionExecutor保存Assistant(Final)；FIFO非空时保存Assistant(Intermediate Continue)、pop一条Steer并从committed ConversationSeed重建AgentLoop segment。

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
→ 如果NotCommitted：证明未写，可安全重试同一draft
→ 如果OutcomeUnknown：poison writer并保守终结当前操作；不在本run内按operation key reopen/replay/lookup；恢复在下次load读committed prefix后按状态处理，不重放Tool
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
- UserMessage append OutcomeUnknown时保守终结、不在本run重试该append，也不创建第二个TurnId；initiating UserMessage可能丢失导致Turn未开始，用户可重新提交；
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
                if let Some(message) = self.pop_next_steer_for_current_turn()? {
                    self.start_steer_composition(message).await?;
                } else {
                    self.start_model_operation().await?;
                }
                return Ok(());
            }
            AgentLoopAction::NeedTools { response, calls } => {
                self.start_tool_operation(response, calls).await?;
                return Ok(());
            }
            AgentLoopAction::Finished { response } => {
                self.handle_candidate_final(response).await?;
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
   → Compaction基于pinned EffectiveModelLimits派生summary budget
   → CompactConversation调用PromptSet和ModelGateway(CompactionSummary request)
→ Executor继续处理SessionIngress的 wakeup 与各 lane
→ OperationResult返回并校验SessionId/TurnId/execution_version/OperationType
   ├─ Model：AgentLoop.accept_model_response → NeedTools或candidate Finished
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
- NeedTools保存含ToolCall的Assistant(Intermediate)并完成整个ToolRound；不因queued Steer丢弃已完成model step；
- Finished只是candidate final。steer queue为空时保存Assistant(Final)；queue非空时保存不含ToolCall的Assistant(Intermediate/Continue)，随后消费一条Steer并继续同一Turn。

## Tool流程

```text
AgentLoop NeedTools { finalized response, calls }
→ 观察`EmergencyControl`最新epoch并重新检查Workspace authorization lease
→ Cancel/revocation获胜：不保存Assistant(Intermediate)，进入Interrupted处理
→ Turn仍Running：取得WorkspaceCommitAuthorization
→ append/apply Assistant(Intermediate)
→ release WorkspaceCommitAuthorization
→ ToolInvocation projection = Started
→ phase = ExecutingTools
→ 启动ToolSet.execute；需要approval时phase = WaitingApproval，需要ask-user时phase = WaitingForUserInput
```

ToolSet流程：

```text
preflight / schema / hook / policy
→ optional ToolExecutionControl.request_approval
→ ToolExecutionControl.record_execution_start
→ sandbox / executor
→ truthful ToolExecutionOutcome
```

若该ToolRound包含首版内建 ask-user route，调度器先处理该 route：

```text
preflight / schema / hook / policy
→ ToolExecutionControl.request_user_question
→ phase = WaitingForUserInput
→ InteractionResolved(UserAnswer | Cancelled | Expired)
→ PreExecution ToolResult candidate
→ ask-user route完成后，才允许同一assistant step的其他ToolCall进入普通调度
```

等待期间不启动 sibling ToolCall，不取得或持有`ToolResourceLocks`、`WorkspaceCommitAuthorization`或其他跨Session副作用资源。UserQuestion解决后，剩余调用才按原始请求顺序和既有资源锁规则继续；不改变ToolResult的稳定返回顺序。

Tool结果处理：

```text
ToolExecutionOutcome[]
→ 每个Completed outcome append/apply role=tool message
→ 任一Abandoned：append/apply ToolAbandoned并进入Turn terminal处理
→ 全部Completed：再次观察`EmergencyControl`和WorkspaceAuthorization lease
→ Cancel/revocation已生效：保留tool messages，不append tool_round_completed，完成Interrupted处理
→ Turn仍Running：取得WorkspaceCommitAuthorization
→ append/apply tool_round_completed
→ release WorkspaceCommitAuthorization
→ conversation projection加入assistant/tool sequence
→ AgentLoop.accept_committed_tool_round
→ current Turn的SteerQueue.pop_front()
   ├─ Some：compose并append/apply一条Steer → 下一次Model
   └─ None：drive_agent_loop
```

规则：

- Assistant(Intermediate)一旦append/apply，当前Tool round必须先得到Completed或Interrupted处理；此后到达的Steer只能排队，不能插入assistant/tool sequence；
- ToolSet按source call order返回outcomes；
- Tool内部允许并发无冲突调用；
- Tool message append OutcomeUnknown时poison writer并保守终结，不在本run重试该append，也不重新执行Tool；该unacked tool message可能丢失，恢复时该round不完整按Abandon处理，由模型重跑工具；
- exact ToolResult必须保存，即使Cancel已经发生；
- outcome unknown不能生成Tool message；
- Cancel/security revocation在tool messages之后、`tool_round_completed`之前生效时，truthful results保持durable但不进入conversation；
- `tool_round_completed`必须exact cover assistant中的全部ToolCall；
- 下一次Model操作必须等待该entry append/apply；
- Tool result progress不是durable Item。

## Interaction流程

Approval 或 UserQuestion request：

```text
ToolExecutionControl.request_approval 或 request_user_question
→ SessionExecutor验证Turn/Item/version
→ append/apply InteractionRequested
→ 注册Pending Interaction和deadline
→ 保存Tool control response sender
→ EventPublisher通知host
→ request handler返回主循环
→ phase = WaitingApproval 或 WaitingForUserInput
→ Tool future等待typed decision/answer
```

Resolution：

```text
ResolveInteraction(expected TurnId, RequestId, resolution_key)
→ 验证family、deadline和Pending state
→ append/apply InteractionResolved
→ 删除deadline
→ 返回typed decision/answer给Tool future
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
- Presentation Adapter只渲染UI-safe Interaction view并通过Runtime facade提交resolution；它不能创建MiniCore未请求的问题、直接写SessionStorage或持有Tool waiter；
- WaitingApproval和WaitingForUserInput时TurnStatus与SessionExecutionState仍为Running；
- `request_approval`/`request_user_question` handler不能在Executor内等待host response；ResolveInteraction或timeout负责完成保存的Tool control response sender；UserQuestion回答只唤醒原Tool future，不创建新Turn。

## Steer流程

Steer属于current Turn，必须携带`expected_turn_id`。它进入按`TurnId`隔离的bounded `SteerQueue<TurnId>` FIFO；Steer不取消Sampling、Compaction、approval、UserQuestion或Tool execution。

```text
Steer request
→ 验证expected Running Turn
→ `SteerQueue<expected_turn_id>.push_back(QueuedMessage)`
→ SteerResult::Queued
```

每次准备开始下一次AgentRun模型调用前，只消费一条：

```text
current Turn的SteerQueue.pop_front()
→ ComposeUserMessage(source = Steer)
→ append/apply UserMessage(source = Steer)
→ AgentLoop.accept_committed_steer，或从updated ConversationSeed重建segment
→ 下一次Model调用
```

安全点：

- 含ToolCall的assistant step：必须先append assistant、全部truthful ToolResult和`tool_round_completed`，再pop一条Steer；
- 无ToolCall的candidate final：queue非空时先把该response保存为model-visible non-terminal Assistant Continue step，再pop一条Steer；
- Compaction：append/apply成功或soft fallback结束后再pop；
- PreparingModel：没有current operation时可以立即pop，但仍走相同composition/append路径。

规则：

- 每个目标 Turn 只提供标准`push_back/pop_front/remove/clear`语义，不增加跨 Turn 的优先级或批量模式；
- 每轮模型请求最多消费一条Steer；剩余消息继续FIFO等待；
- 仍在该 Turn queue中的消息可以按CommandId直接remove；撤销后不重新入队；已经pop并append的Steer不能删除；
- queued Steer在append前不是durable fact，process crash会丢失；
- queue满返回`SteerQueueFull`，不删除旧请求；
- Steer不创建新Turn，不capture新TurnExecutionContext；
- composition失败只丢弃当前已pop消息并发布typed failure，不回到queue；
- final Assistant append后到达的Steer返回ExpectedTurnMismatch或TurnNotRunning。

## FollowUp流程

FollowUp不是current Turn control。

```text
FollowUp request
→ FollowUpQueue.push_back(QueuedMessage)
→ FollowUpResult::Queued
→ 不改变current Turn state或执行路径

current Turn通过自身正常流程terminal后
→ FollowUpQueue.pop_front()
→ 作为普通Submit进入Starting
→ capture新的TurnExecutionContext
```

规则：

- FIFO保留accepted ordering；
- queue满返回`FollowUpQueueFull`，不删除旧请求；
- 每个terminal Turn后最多pop一条FollowUp；其余保持FIFO并等待后续Turn terminal。该 admission 后重新检查 `TurnAdmissionQueue`，连续最多处理一条 FollowUp 后让 external Submit 获得一次公平机会；
- 仍在queue中的FollowUp可以按CommandId直接remove；撤销后不重新入队；
- FollowUp不持久化，restart后不恢复；
- FollowUp不复用旧TurnId、Context、ToolSet、PromptSet、SkillView或Workspace lease；
- previous Turn Completed、Interrupted或Failed后都可以重新admit FollowUp；
- 如果Session变为Archived、Deleted、Unavailable或Agent Disabled，已pop FollowUp admission失败并发布typed rejection；该消息不重新入队；
- PrepareForUnload拒绝新的FollowUp，并清理且明确结束尚未执行的 queued Steer/FollowUp；
- crash-safe FollowUp acknowledgement需要未来storage schema，当前不提供。

## Completed Turn流程

```text
AgentLoop Finished { finalized response }
→ 停止新的Model/Tool操作
→ 观察`EmergencyControl`最新epoch并重新检查Workspace authorization lease
→ Cancel/revocation先处理：进入Interrupted流程
→ 按current Turn的SteerQueue分支
   ├─ 非空：取得WorkspaceCommitAuthorization
   │  → append/apply Assistant(Intermediate Continue)
   │  → release WorkspaceCommitAuthorization
   │  → pop_front一条Steer并append/apply UserMessage
   │  → rebuild AgentLoop segment并继续Running
   └─ 空且lease有效：取得WorkspaceCommitAuthorization
      → state = Finishing
      → 验证无Pending Interaction、Started ToolInvocation或未完成ToolCall Intermediate
      → append/apply Assistant(Final)
      → release WorkspaceCommitAuthorization
      → drop AgentLoop和TurnExecutionContext
      → state = Idle
      → 在FollowUpQueue与TurnAdmissionQueue间执行公平admission或保持Idle
```

Assistant(Final) append是Completed Turn唯一结束线性化点。

## Cancel流程

```text
Cancel(CancelTarget::Turn(expected TurnId))
→ `EmergencyControl`原子验证active target generation并设置sticky cancel
→ 仅触发该target绑定的current operation cancellation token
→ Executor验证current Running Turn
→ execution_version += 1
→ state = Finishing
→ 清理所有仍指向该Turn的queued Steer
→ best-effort cancelContext/Model和可取消Tool操作
→ resolve/cancel Pending Interaction
→ 对尚未执行的Started invocation生成truthful Cancelled ToolResult并append/apply Tool message
→ 对已记录ToolExecutionStarted的调用等待、取消或确认outcome
→ exact result append/apply Tool message
→ only outcome unknown append/apply ToolAbandoned
→ append/apply TurnInterrupted
→ drop AgentLoop和TurnExecutionContext
→ state = Idle
→ 默认保留FollowUp，在terminal后参与正常admission
```

规则：

- Cancel不回滚已经发生的外部副作用；
- Cancel不删除已经append的User/Assistant/Tool entry；
- Cancel只清理目标Turn尚未append的Steer；它不清空FollowUp，也不删除其他尚未开始的Submit；
- cancel epoch发布后到达的同Turn Steer直接返回TurnCancelling/TurnNotRunning；Executor清理的是epoch发布前已经accepted但尚未append的Steer；
- Cancel或PrepareForUnload清理queued input后发布带`CommandId + reason`的`queue_updated` StateEvent；Queued ack不被伪造为已执行，且不会为process-local消息保留跨崩溃completion handle；
- 若产品需要“停止该Session全部工作”，必须定义显式`StopAll`/`ClearQueuedMessages`语义，不能复用单Turn Cancel；
- Cancel不能生成synthetic ToolResult；尚未执行且已确认取消可以生成truthful Cancelled ToolResult；
- Model partial response直接丢弃；
- Cancel response只在TurnInterrupted append/apply完成后返回；
- duplicate Cancel在相同terminal fact后幂等返回当前结果；
- Cancel与Assistant(Final)由Executor实际处理顺序决定；
- Final先append则Cancel返回TurnNotRunning；Cancel先处理则Final candidate丢弃。

Cancel的signal写入不等待任何bounded work lane；target/generation匹配时，signal路径立即触发该target绑定的current operation cancellation token。Executor消费signal后推进shared completion state、更新Session state并返回主循环；后续OperationResult或timeout处理继续cleanup，TurnInterrupted append/apply后完成Cancel response。

Starting期间使用`CancelTarget::Submission(submission_id)`：

```text
matching request still in TurnAdmissionQueue
→ remove queued admission
→ complete Submit response as Rejected(Cancelled)
→ complete Cancel response，current state不变

candidate已进入Starting
→ cancel BuildTurnContext / ComposeUserMessage
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

WorkspaceAuthorizationControl撤销当前lease后，设置`EmergencyControl`中的sticky revocation signal并唤醒Executor；不等待普通 lane 的容量。

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
- 即使Executor尚未消费唤醒，已revoked lease也必须使authorization validation失败；
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
→ release current Turn
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
→ current Model operation返回terminal error并从current_operation移除
→ 确认TurnId/execution_version未变
→ 确认ConversationCheckpoint未变
→ 确认TurnExecutionContext未变
→ 使用相同ModelCallRequest重试
→ increment logical retry_count
```

Retry delay使用timer，不阻塞SessionIngress scheduler或control loop。

logical retry不得通过timeout race留下仍可回传结果的旧本地future。若取消无副作用Model future，必须先安全drop并关闭旧结果路径；provider端可能继续生成或计费只属于delivery/telemetry风险，不允许形成第二个SessionExecutor result。

Steer、Cancel、Compaction或任何model-visible conversation change都会使旧logical retry失效。`RequestOutcomeUnknown`和`StreamInterrupted`的logical retry可能重复provider work或billing；Session policy必须显式限制次数并保留diagnostic，不能宣称exactly-once。

### Tool retry

- pre-execution validation/policy错误可以重新构造新的调用结果，但不自动重新执行同一个side effect；
- ToolExecutionStarted后outcome unknown禁止自动retry；
- SessionWrite NotCommitted可安全重试同一entry write；OutcomeUnknown则poison writer并保守终结，不在本run重试该write，也不重放Tool，恢复在下次load读committed prefix后按状态处理。

## Context Overflow与Compaction连接

具体cut、summary和storage规则见[Compaction架构设计](compaction.md)。SessionExecutor固定以下调用关系：

```text
AgentLoop NeedModel
→ assemble AgentRun context并检查soft pressure/local overflow
→ PromptSet.compaction_summary_assembly_basis()
→ 必要时Compaction.plan
→ TurnExecutionPhase = Compacting
→ RunningOperation::CompactConversation
→ 验证Turn/version/source/authorization/control state
→ append/apply StoredCompaction
→ conversation projection Replace
→ rebuild ConversationSeed和private AgentLoop segment
→ 使用同一个TurnExecutionContext继续Model操作
```

provider ContextOverflow也回到同一流程。同一`source checkpoint + scope frontier`最多一次hard recovery；successful compaction推进basis后，同一Turn可以在`max_compactions_per_turn`内再次compact。

规则：

- Compaction不替换TurnExecutionContext、TurnModelSnapshot或PromptSet；
- 启动Compaction不推进execution_version；成功Replace后推进，Cancel/revocation仍立即推进；
- `ConversationPrefix`压缩pre-Turn history；`ActiveTurnCompletedPrefix`保留exact initiating/Steer UserMessage anchors，并在每个instruction segment内滚动摘要已完成的早期stable units；
- Pending/Started/incomplete ToolRound、explicit protected units和recent exact tail不进入active checkpoint coverage；
- summary output budget必须在plan阶段与pinned model known limits和summary call context空间求交，并进入plan fingerprint；SessionExecutor在append前用`CompactionCommitCandidate`/`ModelCallRequest` proof复核该临时一致性，storage cold replay只验证durable entry关系；
- `Compacting`期间Steer排队，Cancel和Workspace revocation可以取消operation并在append前获胜；
- Compaction result append前必须重新验证source checkpoint和TranscriptFingerprint；
- soft-pressure失败只有在原ModelCallRequest仍exact valid时才能继续；
- hard overflow失败时TurnFailed；successful compact后仍overflow只在存在新可行scope/frontier且次数有余量时再次plan，否则`ContextStillTooLargeAfterCompaction`；
- success后必须重建AgentLoop segment，不能resume携带旧history的run；
- summary operation、plan和retry timer不跨restart恢复。

## PrepareForUnload

`PrepareForUnload`用于graceful shutdown或显式卸载。它不是普通FIFO请求，而是`LifecycleControl`中的sticky stop-admission signal，并携带有限grace deadline：

```text
LifecycleControl.prepare_for_unload(deadline)
→ accepting_requests = false
→ reject new Submit/Steer/FollowUp
→ reject尚未admit的TurnAdmissionQueue请求
→ 清理并明确结束queued Steer/FollowUp
→ 如果Idle：关闭ingress/writer并返回ReadyToUnload
→ 如果Starting/Running/Finishing：保留shared completion generation并允许current work在grace期内自然完成
→ 继续处理EmergencyControl、ResolveInteraction、ToolControl依赖、operation result和timeout
→ grace deadline到期仍未Idle：fail-closed resolve Pending Interaction，触发current submission/Turn cancel
→ truthful Tool outcome与terminal append完成
→ 完成PrepareForUnload completion generation
→ Runtime移除SessionExecutionHandle并释放Executor
```

规则：

- signal handler不能内联等待current Turn完成；
- grace deadline必须由Runtime config给出有限上限，不能依赖可为空的Interaction `expires_at`；
- grace期内current Turn可以正常完成，host也可以ResolveInteraction或显式Cancel；
- deadline到期后必须fail closed，不能让无host、无Interaction deadline或不可取消等待使Unload永久悬挂；
- queued FollowUp默认不会在unload前执行，因为它们在stop-admission时被明确清理；
- SnapshotMailbox在等待期间仍可读最后published view；
- executor释放前ingress关闭、writer closed、progress publisher停止，所有response/completion完成。

## Snapshot

Executor在每次state/projection变更后，把StateEvent cursor推进与带同一`SessionCursor`的immutable `SessionExecutionSnapshotView`原子发布。cursor N的view包含全部`<= N`的StateEvent效果且不包含`> N`的效果。`GetSnapshot`通过`SnapshotMailbox`读取最新published view：

```text
Session lifecycle/readiness
SessionExecutionState
current TurnId/TurnStatus/phase
committed Items
Pending Interactions
pending Submit / queued Steer / FollowUp count
accepting_input / lifecycle stopping state
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

Snapshot不进入mutation/control lane，也不声称与跨 lane 请求形成全局FIFO。它返回某个明确`SessionCursor`对应的完整immutable view；调用方随后从该cursor订阅StateEvent即可补齐snapshot发布后的变化。多个并发snapshot请求可以latest-wins/coalesce，但每个caller都必须得到一个不早于其注册时已published版本的view或typed unavailable error。

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

## Ingress与Operation处理顺序

SessionExecutor是唯一状态修改者。线性化点：

| 操作 | 线性化点 |
| --- | --- |
| Submit开始Turn | initiating UserMessage append |
| Submit进入等待admission | `TurnAdmissionQueue.push_back` |
| Steer/FollowUp排队 | 对应lane的FIFO `push_back` |
| CancelQueuedMessage | 目标消息仍在lane时的原子remove |
| Steer应用 | Steer UserMessage append |
| Interaction request | InteractionRequested append |
| Interaction resolution | InteractionResolved append |
| Tool允许副作用 | ToolExecutionStarted append |
| Tool result确定 | Tool message append |
| Tool round进入conversation | tool_round_completed append |
| Workspace authorization revocation | WorkspaceAuthorizationControl.revoke |
| Cancel触发 | `EmergencyControl` cancel epoch发布；durable完成点仍是terminal append |
| PrepareForUnload | `LifecycleControl` stop-admission epoch发布；完成点是ReadyToUnload |
| Snapshot | immutable view所携带的SessionCursor |
| Turn Completed | Assistant(Final) append |
| Turn Interrupted | TurnInterrupted append |
| Turn Failed | TurnFailed append |

并发规则：

- 只承诺lane内FIFO；不同lane之间不承诺按调用时间或wake时间形成全局顺序；
- operation result与lane同时ready时，先应用emergency/lifecycle signal，再优先完成已经发生的operation outcome和terminal cleanup；
- 处理`ToolExecutionStarted`前必须观察最新emergency epoch；Cancel/revocation先被观察则拒绝start，start append先完成则副作用可以开始并必须保存真实outcome；
- Tool results全部保存后、append `tool_round_completed`前，再观察`EmergencyControl`和authorization lease；
- 处理Finished candidate前观察`EmergencyControl`并重新检查authorization lease；若current Turn的SteerQueue已非空则保存Assistant Continue而不是Final；
- control lane按bounded burst消费；每个burst结束重新poll operation result、deadline和普通admission，防止control flood无限饿死工作；
- terminal后已accepted FollowUp最多获得一次连续优先；若上一Turn由FollowUp启动且TurnAdmissionQueue非空，则先选Submit。未选中的Submit在Session再次Busy时明确结束，不作为隐式FollowUp长期等待；
- 仲裁只读取当前已观察到的lane/signal，不等待未来request；
- workspace-dependent conversation entry的append必须持有WorkspaceCommitAuthorization；它与WorkspaceAuthorizationControl.revoke的先后顺序决定append还是revocation获胜；
- 同时需要两种保护时，先取得非阻塞`TurnControlGate` reservation，再取得`WorkspaceCommitAuthorization`，然后执行一次append/apply；authorization失败则立即释放reservation。TurnControlGate不等待signal，因此不形成反向锁等待；
- 不涉及Workspace authorization的其他append在emergency检查完成后可以继续，并成为该race的线性化结果；
- 每个处理函数结束时state和projections必须保持合法；
- 任何异步operation不能直接修改SessionExecutor字段。

## Multi-Session并发

一个Runtime中的多个Executor独立推进：

```text
Session A: WaitingForUserInput
Session B: Sampling
Session C: WaitingApproval
Session D: ExecutingTools
Session E: Idle
```

必须保证：

- Runtime request按SessionId路由；
- 每个SessionStorage文件只有对应Executor的writer；
- shared services实现并发安全；
- ModelGateway提供global/provider/model/auth-principal并发限制；
- ToolService资源锁使用canonical resource identity，不使用SessionId代替物理资源identity；
- 同一Workspace中的冲突write Tool跨Session串行；
- read-only无冲突Tool可以跨Session并行；
- 一个Session的任一ingress/progress lane达到容量不能耗尽其他Session的lane容量；
- 跨Session仍可能在ModelGateway配额、Tool resource lock和共享I/O处等待；
- Runtime shutdown逐Session设置PrepareForUnload；grace deadline到期后由各Session fail-closed cancel；
- UI current tab不影响后台Session执行。

特别是，Session A 的 UserQuestion waiter 只属于 A 的`SessionExecutor`和`ToolControlQueue`；A 在等待答案时，B/C/D仍可各自推进。共享ModelGateway配额、canonical resource lock或宿主I/O仍可能造成正常竞争，但不会因为UserQuestion本身把其他Session放入同一个等待队列。

## Error分类

```rust
pub enum SessionExecutionError {
    SessionNotOpen,
    SessionNotLoaded,
    SessionNotReady,
    SessionUnavailable,
    ExecutorStopping,
    SessionBusy,
    TurnAdmissionQueueFull,
    SteerQueueFull,
    FollowUpQueueFull,
    InteractionControlQueueFull,
    ToolControlQueueFull,
    SubmissionNotFound,
    NoRunningTurn,
    TurnCancelling,
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
- storage OutcomeUnknown导致writer poison和保守终结（恢复在下次load按committed prefix状态处理），不能转换成Tool outcome unknown；
- Tool failed result与Tool execution infrastructure error分开；
- 各lane的QueueFull是明确backpressure，不重试/丢弃已有请求；EmergencyControl、LifecycleControl和SnapshotMailbox不复用这些容量；
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
old SessionIngress lane contents
queued Submit / Steer / FollowUp
InteractionControl / ToolControl requests
Emergency/Lifecycle completion subscribers和SnapshotMailbox请求
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
- TurnAdmission、Steer、FollowUp、InteractionControl和ToolControl lane分别有容量限制；
- EmergencyControl和LifecycleControl使用O(1) target/state与shared completion generation；SnapshotMailbox只保留latest immutable view，不能无限增长；
- 每个Session最多一个current RunningOperation；旧operation terminal/remove或安全drop前不启动新operation；
- ToolSet内部负责ToolCall并发；
- SessionWriter复用open file handle和buffer；
- 短entry append可以由Executorawait；blocking I/O由SessionWriter内部实现处理；
- streaming progress不写JSONL；
- progress event可以合并；
- snapshot从hot projections构造，不重放完整文件；
- Model/Tool并发配额由共享模块统一执行，不在每个Executor复制全局计数；
- bounded lane达到容量时返回明确错误，不能无限增长；
- lane仲裁的control burst和FollowUp/Submit fairness参数由config给出有限默认值。

## Test Matrix

### State与Admission

- Idle Submit进入Starting；
- Starting期间第二个Submit返回SessionBusy；
- TurnAdmissionQueue满返回TurnAdmissionQueueFull，且不影响EmergencyControl；
- Submit ingress未在本次Idle decision选中且Session被其他消息占用时返回SessionBusy，不跨整个Turn等待；
- CancelTarget::Submission在Context capture和UserMessage composition期间取消candidate；
- Session不是Open/Loaded/Ready时拒绝Submit；
- Context failure不创建Turn；
- stale BuildTurnContext/ComposeUserMessage result在version变化后被忽略；
- Context entry成功、UserMessage失败产生安全orphan；
- Agent disable与UserMessage append的最终顺序；
- UserMessage OutcomeUnknown时保守终结、不在本run重试append也不创建第二个TurnId，用户可重新提交；
- UserMessage append前不调用Model/Tool。

### Model

- NeedModel只使用CommittedConversationView；
- Model运行期间接收Steer；Cancel signal立即触发cancellation token；Snapshot从published view返回；
- provider retry复用相同ModelCallRequest且不改变model identity；
- request delivery outcome unknown和first semantic delta后的stream failure不由Gateway blind retry；
- logical retry只在旧Model operation terminal/remove后启动；不存在两个可向Executor返回结果的同Session Model future；
- Finished candidate在Steer FIFO为空且Cancel/revocation未获胜时才append final；
- Finished candidate遇到queued Steer时保存为model-visible Assistant Continue，再消费一条Steer；
- Steer admission与final commit reservation双向race：Steer先赢转Continue，reservation先赢拒绝Steer；
- NeedTools result无论Steer queue是否非空都先完成assistant/tool/tool_round_completed序列；
- streaming delta丢失不影响final entry；
- transport fallback保持exact model identity；
- cross-model substitution返回typed failure而不是静默继续；
- logical retry_count与Gateway transparent retry count分离。

### Tool

- Assistant(Intermediate)先于Tool执行；
- Cancel在ToolExecutionStarted前获胜时不执行副作用；
- ToolControlQueue已满时Cancel仍可设置EmergencyControl；
- record_execution_start处理前观察到Cancel/revocation epoch时被拒绝；
- ToolExecutionStarted先获胜时Cancel不声称回滚；
- Cancel signal与ToolExecutionStarted controlled reservation双向race均有唯一结果，signal发布不因reservation阻塞；
- approval request append-before-notify；
- resolution append-before-resume；
- UserQuestion request在`ToolExecutionStarted`前append，等待期间不持有资源锁；
- UserQuestion回答恢复同一个Tool future并形成`PreExecution` ToolResult，不伪造副作用；
- Presentation Adapter只消费UI-safe Interaction view并通过Runtime facade Resolve，不能直接写Interaction或持有waiter；
- WaitingForUserInput期间同Session不启动sibling副作用ToolCall，其他Session仍可运行；
- timeout与host resolution first-wins；
- parallel ToolCall按source order保存results；
- 每个ToolResult独立append；
- partial results不进入conversation；
- tool_round_completed后完整round一次进入conversation；
- revocation在tool messages后、tool_round_completed前到达时不把round加入conversation；
- Tool message append OutcomeUnknown时poison writer并保守终结，不重放Tool；
- side-effect outcome unknown产生ToolAbandoned。

### Steer与FollowUp

- 所有Running phase Steer都push_back current Turn对应的普通FIFO；
- Sampling Steer不取消当前Model、不推进execution_version；
- 每个完整assistant/tool step后最多pop_front一条Steer；
- ToolCall step必须在tool_round_completed后再pop Steer；
- 无ToolCall candidate final遇到Steer时保存为Assistant Continue；
- queue内消息按CommandId remove后不重新入队；
- CancelQueuedMessage只删除一条目标Steer或FollowUp，不清空其他消息；
- CancelQueuedMessage remove与lane pop双向race first-wins，pop先赢返回NotQueued且不重新入队；
- Steer queue满返回错误；
- final append前必须确认Steer FIFO为空；
- FollowUp在current Turn terminal后创建新Turn和新Context；
- FollowUp FIFO和QueueFull；
- restart不恢复FollowUp；
- FollowUp后续admission失败发布typed rejection且不重新入队；
- Cancel current Turn清理该Turn全部queued Steer但默认保留FollowUp；
- terminal后FollowUp连续最多admit一条；下一次Idle decision有external Submit时优先Submit，未选中的Submit不跨Turn静默等待；
- PrepareForUnload拒绝pending Submit并明确结束queued Steer/FollowUp；
- PrepareForUnload stop-admission与Submit/Steer/FollowUp try_admit双向race无丢失ack：admission先赢则drain明确结束，stop先赢则立即ExecutorStopping；
- PrepareForUnload grace期仍能处理ResolveInteraction、Cancel、required ToolControl、operation result和timeout；
- unload deadline到期对无deadline Pending Interaction fail-closed并取消active Turn。

### Cancel、Security Revocation与Failure

- Cancel during Context capture；
- Cancel during Sampling；
- Cancel during Compaction；
- Cancel during WaitingApproval；
- Cancel during WaitingForUserInput；
- Cancel during Tool execution before/after execution-start record；
- Cancel保存exact ToolResult；
- Cancel只生成一个TurnInterrupted；
- duplicate Cancel幂等；
- stale CancelTarget/generation不触发current或next Turn token；
- cancel epoch发布后新的同Turn Steer被拒绝，epoch前accepted Steer被清理；
- 普通lane全部满时Cancel仍立即发布sticky signal；
- Cancel不清空FollowUp或其他Submit；
- WorkspaceAuthorizationRevoked during Starting/Compacting/Sampling/WaitingApproval/WaitingForUserInput/ExecutingTools；
- WorkspaceAuthorizationRevoked不等待任何bounded FIFO容量；
- revoked lease/control generation的signal不影响捕获新lease的future Turn；
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
- 一个Session任一ingress lane满不消耗其他Session的lane容量；
- Session A ingress满时Session B的Cancel/Resolve/Submit仍可独立进入其自身lane；
- UI selection变化不影响后台Session；
- shutdown逐Session停止且不串写entry/event。

### Snapshot与Lane Arbitration

- Snapshot view与SessionCursor原子发布；cursor N恰好覆盖全部`<= N`的StateEvent效果；
- SnapshotMailbox在TurnAdmission/ToolControl lane满时仍可返回latest view；
- emergency/lifecycle优先后，operation result不会被持续control burst永久饿死；
- InteractionControl和ToolControl按bounded burst消费后重新poll普通admission。

### Recovery

- restart清除所有async operations和queues；
- unfinished Turn逐entryterminalize；
- existing Tool message保留；
- 不补写tool_round_completed；
- outcome unknown Tool不自动执行；
- 重复load幂等靠committed prefix状态判断（已terminal/已resolved则跳过），不靠recovery operation key；
- terminal entry后Pending/Started被判定为corruption。

## 协同实现顺序

SessionExecutor不以临时model trait独立交付。首个实现必须与ModelGateway和Compaction共享一个vertical-slice harness：

```text
Submit
→ NeedModel
→ PromptSet.assemble(AgentRun)
→ ModelCallRequest::new
→ ModelGateway + ScriptedProviderAdapter
→ assistant/tool result append/apply

context overflow
→ Compaction.plan
→ PromptSet.assemble(CompactionSummary)
→ 同一个ModelCallRequest::new和ModelGateway
→ StoredCompaction append/apply
→ reassemble AgentRun并继续
```

Rig spike可以与scripted harness并行，但production provider adapter冻结前必须完成。SessionExecutor、ModelGateway和Compaction只有在上述两条路径共同通过后才算完成阶段6–8核心交付；职责ownership不因此合并。

## Diagnostics

至少记录：

```text
SessionId / TurnId / ItemId / RequestId
SessionExecutionState / TurnExecutionPhase
execution_version
OperationType和duration
TurnAdmission / Steer / FollowUp / InteractionControl / ToolControl lane depth
EmergencyControl epoch、target generation与completion subscriber count
LifecycleControl state、grace deadline与completion subscriber count
Snapshot published cursor与subscriber count
lane arbitration burst/fairness counters
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
unbounded ingress/progress lane
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

否决原因：UI需要后台Session；SessionStorage、TurnExecutionContext和SessionIngress已经按Session隔离，共享资源可以用明确配额和resource locks协调。
