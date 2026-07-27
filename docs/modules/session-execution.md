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
- ModelGateway private `ProviderAdapter`的最终具体类型；首个production `RigProviderAdapter`只处理provider attempt映射与调用；
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
- Cancel与authority/host `SecurityRevoked`使用sticky、可合并的`EmergencyControl`，不等待普通工作lane的容量；Cancel在线性化sticky epoch后立即返回typed accepted response，PrepareForUnload仍使用sticky lifecycle signal和shared completion generation；
- lane容量、拒绝和仲裁规则在`SessionIngress`内定义；一个Session的lane拥塞不能耗尽另一个Session的容量，但共享Model配额和宿主I/O仍可能形成跨Session等待；
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
- 每个SessionExecutor拥有独立`SessionFileMutationQueue`，只协调该Session同批sibling ToolCall的同文件mutation；
- 跨Session共享Workspace的文件和外部资源冲突不由MiniCore协调，host/user负责worktree、独立Workspace或外部并发控制；
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
    file_mutations: Arc<SessionFileMutationQueue>,
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
    Submit(CommandId),
    Turn(TurnId),
}
```

请求与 lane 的完整映射：

| 请求 | lane | 容量与语义 |
| --- | --- | --- |
| `Submit` | `TurnAdmissionQueue` | bounded FIFO 请求信箱；Executor观察到时Idle则参与admission decision，非Idle立即返回`SessionBusy`；不作为跨Turn排队通道 |
| `Steer` | `SteerQueue<TurnId>` | bounded、按目标 Turn 分组的 FIFO；只作用于 current Turn |
| `FollowUp` | `FollowUpQueue` | bounded FIFO；terminal Turn 后作为新 Turn admission |
| `CancelQueuedMessage` | `InputMailboxControl` | 按 `CommandId` 原子删除 Steer 或 FollowUp 中一条，不清空队列 |
| `ResolveInteraction` | `InteractionControlQueue` | bounded FIFO并配置独立保留容量；只处理已存在的 Pending Interaction |
| `ToolControl` | `ToolControlQueue` | bounded FIFO；approval、UserQuestion、execution-start 等内部 durable control |
| `Cancel` | `EmergencyControl` | target-scoped sticky、可合并signal；在线性化cancel epoch后立即返回`CancelAccepted`，不因普通lane满而延迟 |
| `SecurityRevoked` | `EmergencyControl` | current SessionExecutionHandle-scoped sticky signal；关闭admission，存在candidate/current Turn时绑定target generation |
| `PrepareForUnload` | `LifecycleControl` | sticky stop-admission signal + shared completion generation，可附 grace deadline |
| `GetSnapshot` | `SnapshotMailbox` | latest-wins/coalesced 请求，从 immutable published view 读取 |

`SessionIngress`的外层唤醒机制可以是一个很小的 bounded wake channel，但 wake 只表示“某个 lane 有变化”，不承载请求本身；wake 丢失或合并时由 Executor 重新检查各 lane。因此普通工作容量不会成为 emergency、lifecycle 或 snapshot 的隐藏瓶颈。

表中的`EmergencyControl`是owner-local admission gate与`TurnControlGate`暴露的逻辑signal facet，不是另一个queue或owner。active target registration只由Executor发布。Cancel必须针对immutable `CancelTarget + generation`；SecurityRevoked通过current `SessionExecutionHandle`路由，并在存在candidate/current Turn时绑定其target generation。old/unloaded handle关闭结果路径后，发送方不能修改Session state，也不能把stale signal重定向到new Executor或Turn。

`TurnControlGate`不是queue、actor或第二状态owner，而是`SessionIngress`内部的原子仲裁原语。owner-local admission gate保存security admission状态；TurnControlGate保存active target generation、emergency epoch、Steer admission gate和短commit reservation：

- `try_admit_steer(expected_turn_id)`与`reserve_final_commit(expected_generation)`原子排序。Steer admission先赢时candidate final必须保存为Assistant Continue；final reservation先赢时新Steer返回TurnCancelling/TurnNotRunning；
- `publish_emergency(signal, optional_target_generation)`与admission/`reserve_controlled_append(expected_epoch, kind)`原子排序。Cancel或SecurityRevoked先赢时对应admission/reservation失败；reservation先赢时该次短append获得胜利，signal仍立即sticky并在append后继续cleanup；
- reservation只跨一次短`SessionWriter.append → apply`，不跨Model/Tool I/O、approval或完整Turn；signal发布不阻塞，只记录pending epoch并唤醒Executor；
- 所有以“没有winning emergency”为前置条件的append都必须使用controlled reservation，至少包括initiating UserMessage、Model-produced Assistant、`ToolExecutionStarted`、`tool_round_completed`和active-Turn StoredCompaction；不再叠加第二个Workspace commit permit。
- append返回`NotCommitted`时释放reservation并先观察pending emergency，再决定是否重新reserve并重试同一draft；`OutcomeUnknown`时保持admission closed、poison writer并按既有保守终结规则处理，不能以旧epoch盲重试。

`LifecycleControl`的stop-admission transition与Submit/Steer/FollowUp的`try_admit`原子排序：input admission先赢时PrepareForUnload必须在drain中明确拒绝/清理它；stop-admission先赢时发送方直接得到`ExecutorStopping`。Emergency、required cleanup control和Snapshot不受该gate阻止。

Ingress gate只能依据Executor发布的immutable target/admission generation做容量预留和race排序；领域validation、StateEvent publication与typed `Queued` completion仍由Executor确认。这样lane可以独立背压，但不会产生第二个Session read/write owner。

Admission规则：

- lane capacity由Runtime config决定；lane满时返回对应 typed error（如`TurnAdmissionQueueFull`、`SteerQueueFull`、`FollowUpQueueFull`、`InteractionControlQueueFull`或`ToolControlQueueFull`），不静默丢弃已有输入；
- `Submit`进入`TurnAdmissionQueue`后只等待Executor的下一次state-aware仲裁：仲裁时Session为Idle则参与admission decision，非Idle（含Starting/Running/Finishing）立即返回`SessionBusy`。它不是隐式FollowUp，不跨当前或任何Turn等待；Running期间的用户输入应由UI/CommandSurface路由为Steer或FollowUp。真正的 `Started { turn_id }` 仍在线性化的 initiating UserMessage append/apply 后返回；
- `Steer`/`FollowUp`验证通过并进入各自 FIFO 后立即返回 `Queued`；
- `ResolveInteraction`和`ToolControl`只能对仍然有效的 interaction/turn 生效，过期或不匹配返回 typed rejection；
- `CancelQueuedMessage`只按`CommandId`删除一条仍在 Steer 或 FollowUp lane 的消息；找到即`Cancelled`，找不到统一返回`NotQueued`，不区分从未排队与已经出队；
- `InputMailboxControl.remove(command_id)`与对应lane的`pop_front`使用同一原子同步：remove先赢则消息不执行，pop先赢则返回`NotQueued`且不能把消息重新入队；
- `InputMailboxControl`不直接发布Runtime状态；Executor完成remove、更新immutable snapshot view并发布`queue_updated`后才完成`CancelQueuedMessage` response，因此不产生第二个StateEvent owner；
- `Cancel`重复请求只按相同`CancelTarget + target generation`合并，并立即返回同一accepted cancel epoch；EmergencyControl不保存completion sender，stale target不得触发当前或下一Turn的cancellation token；
- Emergency signal在目标terminal/取消完成后retire，新的Turn使用新的target generation和CancellationToken；
- SecurityRevoked signal绑定current `SessionExecutionHandle`，并在存在candidate/current Turn时绑定`CancelTarget + target generation`；重新resolve完成后retire，不得影响future Turn；
- `PrepareForUnload`重复请求订阅同一个completion generation，effective grace deadline取现有与新请求的最早值，后续请求不能延长shutdown；
- Snapshot 请求不改变任何 mutation lane 的顺序，不等待普通 queue 排空。

长流程 response 不让 ingress handler 等待完整 operation：

- Submit response 保存在 candidate/admission record 中，UserMessage append/apply 后完成；
- Cancel response在sticky cancel epoch成功发布后立即返回；`TurnInterrupted`/其他terminal fact由后续StateEvent和Snapshot表达；
- PrepareForUnload response等待lifecycle completion generation，进入 Unloaded 前完成；
- queued Steer/FollowUp 不保存跨崩溃 completion handle；真正 append、新 Turn start 和拒绝由 StateEvent/typed outcome 表达。

## Tool Execution Control

Tool执行使用crate-private `ToolExecutionControl`请求SessionExecutor完成durable操作。权威 interface（包括 `request_approval`、`request_user_question` 和 `record_execution_start`）只在[Tool子系统](tools.md#tool-execution-control)定义；本模块不重复定义 trait，避免两个窄 interface 漂移。

实现把Tool control request写入`ToolControlQueue`。Tool future发送request后等待typed response，但不持有SessionExecutor的锁或mutable reference。`request_approval`与`request_user_question`都由同一SessionExecutor完成InteractionRequested/Resolved的append/apply与waiter唤醒；前者返回typed approval，后者返回typed UserQuestion answer。

`ToolControlQueue`只保证自身FIFO，不与Steer或Submit建立全局顺序。安全性依赖state-aware arbitration和append线性化点：处理任何`record_execution_start`前，Executor必须先观察`EmergencyControl`的最新epoch并重新验证Turn、cancellation和authorization；若emergency已生效，拒绝新的execution start。若`ToolExecutionStarted`已经append/apply，则该副作用获得真实线性化结果，后续Cancel只能best-effort取消并确认outcome。

SessionExecutor处理Tool control request时必须重新验证：

- expected SessionId和TurnId；
- execution version；
- ItemId/ToolCallId仍属于current Running Turn；
- Turn没有Cancel或SecurityRevoked；
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
    submit_command_id: CommandId,
    turn_id: TurnId,
    execution_version: u64,
    intent: PromptIntent,
    context: Option<Arc<TurnExecutionContext>>,
    response: oneshot::Sender<Result<SubmitResult, SessionExecutionError>>,
    cancellation: CancellationToken,
}
```

Candidate不是领域Turn。initiating UserMessage append前，Executor复制Submit envelope的`CommandId`并使用`CancelTarget::Submit(command_id)`定位它；不生成第二个submission identity。

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
- SecurityRevoked；
- Turn进入terminal处理。

递增version不能撤销已经append的entry，也不能丢弃已经可能发生副作用的Tool outcome。

## Running Operation

SessionExecutor最多持有一个`current_operation: Option<RunningOperation>`，它是当前逻辑Model/Tool/Compaction工作的唯一execution-local状态。每个ModelGateway operation最多包含一个provider attempt；CurrentTurnExecution不保存并列的ModelAttemptState。`GenerateModelResponse` operation的scoped progress adapter持有`content_index → StreamingItem`的operation-local map；其terminal `OperationOutput::Model`包装Gateway的`ModelCallResult`和ItemId映射，ModelGateway interface保持不变。该adapter不直接修改CurrentTurnExecution或projection。operation future由主循环直接poll，不detach成可以在owner不知情时回传结果的后台task。等待Model/Tool I/O时，Executor仍可通过`select!`处理SessionIngress的各 lane；“处理一个 ingress 请求”不等于“并行启动第二个logical operation”。

新operation只能在旧operation满足以下任一条件后启动：

```text
terminal OperationResult已处理并移除
或
对无外部副作用的future执行安全drop，且旧结果通道已关闭
```

Model、retry delay、Context、composition和未开始副作用的Tool可以在Cancel后安全drop。Tool越过`ToolExecutionStarted`后必须保留为current operation直到exact outcome或明确Abandoned settlement；期间不启动下一次Model/Tool operation。

```rust
pub(crate) enum ModelRetryResume {
    AgentRun,
    CompactionSummary {
        source: ConversationCheckpoint,
        scope: CompactionScope,
        plan_fingerprint: CompactionPlanFingerprint,
    },
}

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
        request: Arc<ModelCallRequest>,
        logical_retry_count: u8,
        cancel: CancellationToken,
    },
    WaitForModelRetry {
        turn_id: TurnId,
        execution_version: u64,
        request: Arc<ModelCallRequest>,
        logical_retry_count: u8,
        ready_at: Instant,
        resume: ModelRetryResume,
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
        summary_request: Option<Arc<ModelCallRequest>>,
        logical_retry_count: u8,
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
    }
}
```

实现要求：

- 不能在ingress handler中内联等待完整Model request、完整Tool execution或用户approval；主循环通过`select!`同时poll唯一current operation和lane wakeup；LifecycleControl的grace deadline通过对应wakeup推进；
- wakeup只触发一次state-aware arbitration；handler按lane取出一条或合并signal，然后更新state、保存response sender或启动operation，再返回主循环；
- 可以等待一次短SessionWriter append取得确定结果；
- 如果文件adapter使用blocking syscall，SessionWriter implementation可以在内部offload I/O，但不能产生第二个Session semantic owner；
- ingress和operation result的状态修改只发生在SessionExecutor；
- progress event不进入任何mutation/control lane。

### Lane arbitration

各 lane 没有隐含的全局 FIFO。Executor在每个安全点按当前状态仲裁，固定优先级为：

1. `EmergencyControl`与`LifecycleControl`：先应用新的Cancel/SecurityRevoked epoch，停止admission；卸载deadline到期时fail-closed；
2. 已完成的operation result和terminal cleanup；它们负责兑现已经发生的durable work；
3. `InteractionControlQueue`：只处理当前 Pending Interaction 的 resolution，append/apply 后再恢复 Tool；
4. 当前 Tool round 需要的 `ToolControlQueue` 请求；每次只消费 bounded burst，避免 control flood 持续饿死普通 admission；
5. 当前 Turn 的安全点消费至多一条 `SteerQueue` 消息；
6. Turn terminal 后在 `FollowUpQueue` 与信箱中此刻已到达的 `TurnAdmissionQueue` Submit 之间做一次公平 admission：已接受的 FollowUp最多获得一次连续优先；若上一Turn由FollowUp启动且此刻有external Submit，则先选Submit。本次decision未选中的Submit立即返回`SessionBusy`；decision之后到达的Submit按Session当时状态处理（非Idle立即`SessionBusy`）；
7. `SnapshotMailbox`独立读取最新 immutable published view，不等待上述 lane。

`EmergencyControl`的检查点至少位于启动新Model、预留或继续file mutation ticket、`ToolExecutionStarted` append前，以及terminal Assistant/`tool_round_completed` append前。仲裁只处理当前已观察到的signal，不等待未来请求；因此每个安全点仍有有限、可测试的边界。

## AgentLoop Interface

AgentLoop是自研的crate-private concrete implementation（[ADR 0115](../adr/0115-agent-loop-is-first-party-state-machine.md)），不定义public trait。

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

`accept_model_response`只接收ModelGateway已经通过Response Validation的`FinalizedAssistantResponse`。Provider wire、finish/content consistency和OutputContract错误不会进入AgentLoop：

```text
validated response含ToolCall → PendingToolRound → NeedTools
validated response无ToolCall → EmittedCandidate → Finished
```

合法ToolCall只可能来自`output_contract = None`且request tools非空的调用；`ToolCalls`无call、`Stop/Refused`带call、Length、ContentFiltered、empty response、UnexpectedToolCall和invalid Structured output已经由ModelGateway返回typed error。AgentLoop仍校验自身state transition、trusted ToolRound coverage和candidate one-shot consumption，但不重新解释Provider错误。

AgentLoop是自研的同步sans-I/O状态机，不由Rig或其他SDK驱动（ADR 0115）。它不得：

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

普通Submit只在`Idle + Open + Loaded + Ready + accepting_requests`时接受；Executor处理Submit时Session非Idle则立即返回`SessionBusy`，不跨当前Turn等待。Turn Running期间的用户输入由UI/CommandSurface路由为`Steer`（交互式默认）或`FollowUp`；`TurnAdmissionQueue`只是admission请求信箱，其中的Submit至多等待到下一次state-aware仲裁被观察，不构成隐式FollowUp。

```text
Submit
→ 验证Session lifecycle/load/readiness
→ state = Starting
→ 保存Submit CommandId，创建candidate TurnId和execution_version=1
→ 启动BuildTurnContext
→ Context result identity/version validation
→ 启动ComposeUserMessage(source=Input)
→ composition result identity/version validation
→ 与Agent status update串行化
→ final AgentStatus = Enabled check
→ reserve TurnControlGate controlled append
→ append/apply TurnContext
→ append/apply UserMessage(source=Input)
→ release controlled reservation
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
→ AgentRun GenerateModelResponse期间首次出现message/reasoning content_index
   → 分配稳定ItemId并创建StreamingItem
   → 发布agent_message_started / reasoning_started ProgressEvent
→ AgentRun后续delta更新同一StreamingItem并使用同一ItemId发布ProgressEvent
→ OperationResult返回并校验SessionId/TurnId/execution_version/OperationType
   ├─ AgentRun Model：finalized text/reasoning生成FinalItemCandidate → AgentLoop.accept_model_response → NeedTools或candidate Finished
   │  → Assistant entry append/apply
   │  → 按finalized content顺序一次投影/publish：Reasoning/AgentMessage Completed、ToolInvocation Started
   │  → 清理matching StreamingItem
   ├─ CompactionSummary：不创建StreamingItem/ItemId → revalidate → append/apply Replace → rebuild AgentLoop → reassemble
   ├─ delivery-safe terminal error：移除旧operation → WaitForModelRetry → timer后重新校验 → 复用同一request启动下一operation
   └─ model response error：丢弃StreamingItem → 不调用AgentLoop/ToolSet → non-retryable Model TurnFailure
```

规则：

- PromptSet是唯一context assembly实现；
- ModelGateway只接收validated `ModelCallRequest`；该request的唯一model-visible input是`AssembledModelContext`；
- ModelGateway single attempt失败后，Session logical retry复用同一个immutable ModelCallRequest；
- MVP不执行provider transparent retry或transport fallback；
- active Turn内不允许transparent cross-model fallback；
- logical retry只允许在ConversationCheckpoint、TurnExecutionContext、purpose、output contract和effective max_output_tokens均未改变时发生；
- `StreamingItem`只属于当前AgentRun `GenerateModelResponse` operation，按provider-neutral `content_index`关联，不进入CompactionSummary、CurrentTurnExecution、TurnExecutionContext、SessionStorage或Snapshot authoritative view；
- message/reasoning在首个streamed content update分配稳定ItemId，started与delta通过ProgressEventPublisher发布；
- partial response和StreamingItem都不是AgentMessage/Reasoning Item；
- provider terminal success只生成FinalItemCandidate；从未产生progress的final content在normalization时分配ItemId。candidate成功append/apply后，Reasoning/AgentMessage成为Completed Item，ToolCall成为Started ToolInvocation；同一assistant entry的new-Item StateEvent严格按finalized Reasoning/Text/ToolCall content顺序发布；
- terminal error、Cancel或validation failure直接丢弃StreamingItem，不发布synthetic completed；
- Model result只有在AgentLoop返回NeedTools或Finished后才决定对应entry类型；
- NeedTools保存含ToolCall的Assistant(Intermediate)并完成整个ToolRound；不因queued Steer丢弃已完成model step；
- Finished只是candidate final。steer queue为空时保存Assistant(Final)；queue非空时保存不含ToolCall的Assistant(Intermediate/Continue)，随后消费一条Steer并继续同一Turn。

## Tool流程

```text
AgentLoop NeedTools { finalized response, calls }
→ 观察`EmergencyControl`最新epoch并reserve controlled append
→ Cancel/SecurityRevoked获胜：不保存Assistant(Intermediate)，进入Interrupted处理
→ append/apply Assistant(Intermediate)
→ release controlled reservation
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
→ InteractionResolved(UserAnswer | Cancelled)
→ PreExecution ToolResult candidate
→ ask-user route完成后，才允许同一assistant step的其他ToolCall进入普通调度
```

等待期间不启动 sibling ToolCall，不预留file mutation ticket，也不持有TurnControl reservation。UserQuestion解决后，剩余调用才按原始请求顺序和既有mutation queue/Serial批次规则继续；不改变ToolResult的稳定返回顺序。

Tool结果处理：

```text
ToolExecutionOutcome[]
→ 每个Completed outcome append/apply role=tool message
→ 任一Abandoned：append/apply ToolAbandoned并进入Turn terminal处理
→ 全部Completed：再次观察`EmergencyControl`并reserve controlled append
→ Cancel/SecurityRevoked已生效：保留tool messages，不append tool_round_completed，完成Interrupted处理
→ append/apply tool_round_completed
→ release controlled reservation
→ conversation projection加入assistant/tool sequence
→ AgentLoop.accept_committed_tool_round
→ current Turn的SteerQueue.pop_front()
   ├─ Some：compose并append/apply一条Steer → 下一次Model
   └─ None：drive_agent_loop
```

规则：

- Assistant(Intermediate)一旦append/apply，当前Tool round必须先得到Completed或Interrupted处理；此后到达的Steer只能排队，不能插入assistant/tool sequence；
- ToolSet按source call order返回outcomes，Tool内部允许并发无冲突调用；先完成的Tool可以先通过process-local progress更新UI activity，但ToolInvocation的authoritative Completed只在matching Tool message append/apply后成立；
- Tool terminal状态按ItemId更新assistant ToolCall创建的原位置，不按完成时间移动Item；
- Tool message append OutcomeUnknown时poison writer并保守终结，不在本run重试该append，也不重新执行Tool；该unacked tool message可能丢失，恢复时该round不完整按Abandon处理，由模型重跑工具；
- exact ToolResult必须保存，即使Cancel已经发生；
- outcome unknown不能生成Tool message；
- Cancel/SecurityRevoked在tool messages之后、`tool_round_completed`之前生效时，truthful results保持durable但不进入conversation；
- `tool_round_completed`必须exact cover assistant中的全部ToolCall；
- 下一次Model操作必须等待该entry append/apply；
- Tool result progress不是durable Item。

## Interaction流程

Approval 或 UserQuestion request：

```text
ToolExecutionControl.request_approval 或 request_user_question
→ SessionExecutor验证Turn/Item/version
→ append/apply InteractionRequested
→ 注册Pending Interaction
→ 保存Tool control response sender
→ EventPublisher通知host
→ request handler返回主循环
→ phase = WaitingApproval 或 WaitingForUserInput
→ Tool future等待typed decision/answer
```

Resolution：

```text
ResolveInteraction(expected TurnId, RequestId, resolution_key)
→ 验证family和Pending state
→ append/apply InteractionResolved
→ 返回typed decision/answer给Tool future
→ request response成功
```

等待不会因elapsed time自动结束。Pending Interaction只由Host resolution、显式Cancel、Turn terminal cleanup、Unload/shutdown或restart recovery关闭。

规则：

- request notification允许at-least-once delivery；
- reconnect使用相同RequestId；
- same resolution_key重试幂等；
- different key在Resolved后返回AlreadyResolved；
- disconnect、暂时没有subscriber或长时间无回答都不自动关闭Interaction，也不产生默认Deny；
- embedding无法展示Interaction时必须选择明确的non-interactive policy或显式Cancel，不能靠时间推断用户决定；
- Presentation Adapter只渲染UI-safe Interaction view并通过Runtime facade提交resolution；它不能创建MiniCore未请求的问题、直接写SessionStorage或持有Tool waiter；
- WaitingApproval和WaitingForUserInput时TurnStatus与SessionExecutionState仍为Running；
- `request_approval`/`request_user_question` handler不能在Executor内等待host response；ResolveInteraction或生命周期cleanup负责完成保存的Tool control response sender；UserQuestion回答只唤醒原Tool future，不创建新Turn。

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
- FollowUp不复用旧TurnId、Context、ToolSet、PromptSet、SkillView或WorkspaceSnapshot；
- previous Turn Completed、Interrupted或Failed后都可以重新admit FollowUp；
- 如果Session变为Archived、Deleted、Unavailable或Agent Disabled，已pop FollowUp admission失败并发布typed rejection；该消息不重新入队；
- PrepareForUnload拒绝新的FollowUp，并清理且明确结束尚未执行的 queued Steer/FollowUp；
- crash-safe FollowUp acknowledgement需要未来storage schema，当前不提供。

## Completed Turn流程

```text
AgentLoop Finished { finalized response }
→ 停止新的Model/Tool操作
→ 观察`EmergencyControl`最新epoch
→ Cancel/SecurityRevoked先处理：进入Interrupted流程
→ 按current Turn的SteerQueue分支
   ├─ 非空：reserve controlled append
   │  → append/apply Assistant(Intermediate Continue)
   │  → release reservation
   │  → pop_front一条Steer，重新reserve并append/apply UserMessage
   │  → rebuild AgentLoop segment并继续Running
   └─ 空：reserve final commit
      → state = Finishing
      → 验证无Pending Interaction、Started ToolInvocation或未完成ToolCall Intermediate
      → append/apply Assistant(Final)
      → release final reservation
      → drop AgentLoop和TurnExecutionContext
      → state = Idle
      → 在FollowUpQueue与TurnAdmissionQueue间执行公平admission或保持Idle
```

Assistant(Final) append是Completed Turn唯一结束线性化点。

## Cancel流程

```text
Cancel(CancelTarget::Turn(expected TurnId))
→ `EmergencyControl`通过`TurnControlGate`原子验证active target generation仍可取消且final reservation未赢
→ 设置sticky cancel epoch并触发该target绑定的current operation cancellation token
→ 立即返回CancelAccepted { target, cancel_epoch }
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
- Cancel response只确认sticky epoch已发布，不确认Tool已停止或Turn已terminal；
- duplicate Cancel幂等返回同一accepted target/epoch；
- Cancel与Assistant(Final)由`TurnControlGate`上的cancel epoch/final append reservation first-wins决定；
- final reservation先赢则Cancel返回typed terminal/transition error；Cancel先赢并返回accepted则Final candidate必须丢弃。

Cancel的signal写入不等待任何bounded work lane；target/generation仍可取消且对应commit reservation未赢时，signal路径立即触发该target绑定的current operation cancellation token并返回`CancelAccepted`。reservation先赢时直接返回typed transition/terminal error。Executor消费signal后递增execution_version、进入Finishing并发布新snapshot/event；后续OperationResult或terminal cleanup继续推进，TurnInterrupted append/apply后发布最终StateEvent。

Cancel后不立即启动同Session新Turn。Session未进入Unload stop-admission时，Finishing期间仍接受FollowUp进入bounded FIFO并返回Queued；普通Submit返回SessionBusy，新Steer返回TurnCancelling/TurnNotRunning。旧Turnterminal后，FollowUp按既有公平admission规则开启下一Turn。

Turn创建前使用`CancelTarget::Submit(command_id)`：

Cancel epoch与initiating UserMessage append reservation使用同一`TurnControlGate` first-wins。cancel先赢才返回`CancelAccepted`并保证不创建领域Turn；initiating reservation先赢时Cancel返回typed stale/transition error，原Submit完成append/apply并返回`TurnStarted`。

```text
matching request still in TurnAdmissionQueue
→ remove queued admission
→ complete Submit response as Rejected(Cancelled)
→ Cancel response已在cancel epoch发布时返回，current state不变

candidate已进入Starting
→ cancel BuildTurnContext / ComposeUserMessage
→ discard candidate Context/message
→ complete Submit response as Rejected(Cancelled)
→ Cancel response已返回
→ state = Idle
```

## Authority Security Interruption

WorkspaceAuthority或host发布hard restriction后，Runtime通过current loaded map向受影响`SessionExecutionHandle`设置sticky `EmergencyControl::SecurityRevoked`；存在candidate/current Turn时同时绑定target generation。它不是Workspace definition update，不原地替换active Snapshot，也不携带lease identity。

```text
SecurityRevoked(optional_target_generation)
→ owner-local admission gate立即拒绝new Turn admission
→ Idle：旧Snapshot停止admit，直接重新resolve
→ Starting：取消candidate，不创建领域Turn，然后重新resolve
→ Running/Finishing：TurnControlGate signal first-wins后拒绝新的append/Tool/Model start
→ execution_version += 1，state = Finishing
→ cancel Context/Model和可取消Tool operation
→ 已开始副作用的Tool按Cancel规则确认exact outcome或ToolAbandoned
→ 不append缺失的tool_round_completed
→ append/apply TurnInterrupted(SecurityRevoked)
→ release current Turn（若存在）
→ 使用current durable Workspace definition/current authority重新resolve
→ Ready + Idle，或Unavailable
```

规则：

- `SecurityRevoked`与controlled append、final reservation和`ToolExecutionStarted`只使用现有`TurnControlGate` epoch排序；signal先赢则operation不得开始，reservation/start先赢则短append或已开始副作用完成truthful settlement；
- signal不等待bounded lane，不需要第二个Workspace permit或handle registry；
- interruption不能撤回provider已经看到的内容、已经打开的OS handle或已发生副作用；
- 已append但conversation-hidden的assistant/tool entries保持durable；
- Idle/Starting也必须失效旧Snapshot：Idle直接resolve，Starting取消candidate后resolve，不等待创建Turn；
- FollowUp可在Finishing期间排队，但只有terminal、重新resolve且Ready后才能admit；
- signal是process-local control fact。restart无法从durable evidence证明security cause时使用`HostRestart`或`RecoveryContextUnavailable`，不猜测`SecurityRevoked`。

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

### Provider attempt

MVP中ModelGateway每个operation最多执行一个provider attempt；Rig和底层provider SDK automatic retry固定为0。Gateway不在401后重发，不执行WebSocket → HTTP fallback，也不持有retry timer。request前credential resolve/refresh仍由Gateway负责；provider error以typed terminal result返回SessionExecutor。

### Logical model retry

由SessionExecutor决定：

```text
Model/CompactionSummary operation retryable failure
→ current operation返回terminal error并从current_operation移除
→ 确认TurnId/execution_version未变
→ 确认ConversationCheckpoint未变
→ 确认TurnExecutionContext未变
→ increment logical_retry_count并检查purpose上限
→ 发布logical model_retry_scheduled，丢弃上一AgentRun operation的StreamingItem
→ 创建WaitForModelRetry
→ timer terminal/remove后使用相同ModelCallRequest启动下一operation
```

Retry delay使用`RunningOperation::WaitForModelRetry`中的timer，不阻塞SessionIngress scheduler或control loop。旧Model/Compaction operation必须terminal并移除后才能创建该delay operation；delay terminal并移除后才能创建下一次Model/Compaction operation，因此`current_operation`仍是唯一execution-local work state。

delay operation只持有同一个`Arc<ModelCallRequest>`、当前`logical_retry_count`、`ready_at`和恢复下一operation所需的最小typed `ModelRetryResume`，不分配attempt ID，也不是`ModelAttemptState`。AgentRun resume没有额外数据；CompactionSummary resume只保留source、scope和plan fingerprint，summary directive、assembled context与request proof继续由同一个immutable request提供。timer到期后必须先处理已经到达的Cancel/SecurityRevoked/Steer，再决定是否启动下一次single-attempt Gateway operation。成功response、新ModelCallRequest或失效control会清除该调用链计数。

logical retry不得通过timeout race留下仍可回传结果的旧本地future。若取消无副作用Model future，必须先安全drop并关闭旧结果路径；provider端可能继续生成或计费只属于delivery/telemetry风险，不允许形成第二个SessionExecutor result。

默认AgentRun最多3次logical retry，backoff为2秒、4秒、8秒；一次成功finalized response或新的ModelCallRequest开始后计数重置。自动retry必须同时满足Gateway已证明`NotSent`或`RejectedBeforeExecution`，且reason是`Timeout`、`TransportUnavailable`、`ProviderUnavailable`，或typed `Retry-After <= 60s`的`RateLimited`；实际delay取指数backoff与provider hint的较大值。`AcceptedNoOutput`没有明确pre-execution rejection proof时按`RequestOutcomeUnknown`处理。超过60秒、`RequestOutcomeUnknown`、`StreamInterrupted`、认证、quota、配置、安全、`UnexpectedToolCall`、`InvalidStructuredOutput`、`InvalidProviderResponse`和`IncompleteResponse`默认不自动retry。`ContextOverflow`进入Compaction。

Steer、Cancel、成功Compaction Replace或任何model-visible conversation change都会使旧logical retry失效。任何logical retry都可能重复provider work或billing，不能宣称exactly-once。完整policy见[ADR 0119](../adr/0119-model-calls-use-session-logical-retries.md)。

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
- 启动Compaction不推进execution_version；成功Replace后推进，Cancel/SecurityRevoked仍立即推进；
- `ConversationPrefix`压缩pre-Turn history；`ActiveTurnCompletedPrefix`保留exact initiating/Steer UserMessage anchors，并在每个instruction segment内滚动摘要已完成的早期stable units；
- Pending/Started/incomplete ToolRound、explicit protected units和recent exact tail不进入active checkpoint coverage；
- summary output budget必须在plan阶段与pinned model known limits和summary call context空间求交，并进入plan fingerprint；SessionExecutor在append前用`CompactionCommitCandidate`/`ModelCallRequest` proof复核该临时一致性，storage cold replay只验证durable entry关系；
- `Compacting`期间Steer排队，Cancel和SecurityRevoked可以取消operation并在append前获胜；
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
→ 继续处理EmergencyControl、ResolveInteraction、ToolControl依赖和operation result
→ grace deadline到期仍未Idle：触发current pre-Turn Submit或Turn cancel，并以Cancelled关闭Pending Interaction
→ truthful Tool outcome与terminal append完成
→ 完成PrepareForUnload completion generation
→ Runtime移除SessionExecutionHandle并释放Executor
```

规则：

- signal handler不能内联等待current Turn完成；
- grace deadline必须由Runtime config给出有限上限；它是显式Unload的生命周期deadline，不是Interaction inactivity timeout；
- grace期内current Turn可以正常完成，host也可以ResolveInteraction或显式Cancel；
- deadline到期后必须Cancel active work并以`Cancelled(TurnTerminated)`关闭Pending Interaction，不能让等待阻止Unload；
- queued FollowUp默认不会在unload前执行，因为它们在stop-admission时被明确清理；
- SnapshotMailbox在等待期间仍可读最后published view；
- executor释放前ingress关闭、writer closed、progress publisher停止，所有response/completion完成。

## Snapshot

Executor在每次state/projection变更后发布新的immutable `SessionExecutionSnapshotView`。`GetSnapshot`通过`SnapshotMailbox`读取latest published view；持续观察通过Runtime Interface的snapshot-first subscription完成：subscriber注册与初始view capture必须在同一publication synchronization内原子完成，随后只接收该点之后的实时事件。view至少包含：

```text
Session lifecycle/readiness
SessionExecutionState
current TurnId/TurnStatus/phase
committed Items（selected path + assistant content canonical order）
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

Snapshot不进入mutation/control lane，也不声称与跨lane请求形成全局FIFO。多个并发snapshot请求可以latest-wins/coalesce，但每个caller都必须得到一个不早于其注册时已published版本的view或typed unavailable error。它是live observer baseline，不是durable execution checkpoint。subscription背压或disconnect后不重放旧事件；host重新subscribe并从新Snapshot恢复。Runtime process restart先replay JSONL并conservative terminalize unfinished Turn，随后Snapshot只反映Idle/Ready/Unavailable等恢复结果。

## Progress Event

`ProgressEventPublisher`处理高频、非durable事件：

```text
agent message / reasoning started
assistant text / reasoning delta
Tool stdout/progress
logical model retry scheduled
phase change notification
```

`StreamingItem`和`FinalItemCandidate`的权威形状见[Turn、Item与Interaction](turn-item-interaction.md#streaming与observer-event)。SessionExecutor只为AgentRun `GenerateModelResponse`创建Item progress adapter；adapter先无损更新`content_index → StreamingItem`映射，再尝试向可合并/丢弃的Host ProgressEvent queue发布，并在`OperationOutput::Model`中归还ItemId映射。CompactionSummary使用不创建ItemId的compaction progress adapter。ModelGateway只发布provider-neutral content index/delta，不创建MiniCore ItemId。

规则：

- 使用独立bounded queue；
- started、delta和append/apply后的completed使用同一ItemId；Host漏掉started时可由首个delta创建临时view；跨Item progress first-seen顺序仅用于provisional presentation，不是durable Item order；
- 可以按SessionId/TurnId/ItemId合并连续delta；
- queue满时允许丢弃中间progress；StateEvent通道无法继续时关闭subscription，由新Snapshot恢复，不把final event称为durable log；
- committed-derived final StateEvent从append/apply后的entry生成，包含完整final view；process-local final view由当前Executor snapshot校正；
- append失败不能发布item_completed；logical retry发布`model_retry_scheduled`并要求Host清理上一Model operation的临时view，Turn terminal或新Snapshot提供最终校正；
- Host收到同ItemId completed或Turn terminal后忽略独立progress queue中迟到的started/delta；
- progress publisher失败不影响SessionStorage或Turn terminal；
- 状态变化进入per-session StateEvent；ProgressEvent可以合并/丢弃。subscription背压或disconnect时关闭stream并重新snapshot，完整规则见[Runtime Interface](runtime-interface.md)。

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
| Authority/host security interruption | `EmergencyControl::SecurityRevoked` epoch publication；durable completion仍是TurnInterrupted append |
| Cancel触发 | `EmergencyControl` cancel epoch发布；durable完成点仍是terminal append |
| PrepareForUnload | `LifecycleControl` stop-admission epoch发布；完成点是ReadyToUnload |
| Snapshot | latest immutable view读取；subscription初始Snapshot与subscriber注册原子完成 |
| Turn Completed | Assistant(Final) append |
| Turn Interrupted | TurnInterrupted append |
| Turn Failed | TurnFailed append |

并发规则：

- 只承诺lane内FIFO；不同lane之间不承诺按调用时间或wake时间形成全局顺序；
- operation result与lane同时ready时，先应用emergency/lifecycle signal，再优先完成已经发生的operation outcome和terminal cleanup；
- 处理`ToolExecutionStarted`前必须观察最新emergency epoch；Cancel/SecurityRevoked先被观察则拒绝start，start append先完成则副作用可以开始并必须保存真实outcome；
- Tool results全部保存后、append `tool_round_completed`前，再观察`EmergencyControl`并reserve controlled append；
- 处理Finished candidate前观察`EmergencyControl`；若current Turn的SteerQueue已非空则保存Assistant Continue而不是Final；
- control lane按bounded burst消费；每个burst结束重新poll operation result、lifecycle signal和普通admission，防止control flood无限饿死工作；
- terminal后已accepted FollowUp最多获得一次连续优先；若上一Turn由FollowUp启动且此刻信箱中有external Submit，则先选Submit。本次decision未选中的Submit立即返回SessionBusy；decision之后到达的Submit按Session当时状态处理，不作为隐式FollowUp等待；
- 仲裁只读取当前已观察到的lane/signal，不等待未来request；
- 依赖current Turn继续有效的conversation append统一使用`TurnControlGate` reservation；它与Cancel/SecurityRevoked signal的先后顺序决定append还是interruption获胜；
- TurnControlGate reservation是唯一control commit permit，不再叠加Workspace authorization permit；
- cleanup/terminal append按对应Finishing规则执行，不因security signal重复产生terminal事实；
- 每个处理函数结束时state和projections必须保持合法；
- 任何异步operation不能直接修改SessionExecutor字段。

### 异步同步纪律

MiniCore不建立覆盖全Runtime的lock-rank系统。并发安全依赖single owner、短状态guard、non-blocking signal/reservation和语义明确的typed permit（[ADR 0117](../adr/0117-async-synchronization-uses-single-owner-and-typed-permits.md)）：

- 普通Mutex/RwLock guard不得跨任意`.await`、跨owner调用、event publication、fan-out或host callback；
- Agent/Session lifecycle状态只在短同步作用域内读取、clone、CAS或替换；durable mutation完成后先释放gate，再通知loaded Executor；
- 有意跨await的一次一个操作使用typed permit/semaphore，只允许覆盖文档指定的bounded append/apply sequence；
- Agent start commit使用私有组合操作固定final AgentStatus check、controlled append和permit释放，调用方不能自行嵌套同步原语；
- controlled append使用私有helper固定`TurnControl reservation → append/apply → release`；
- Model permit、file mutation ticket、Tool/Sandbox、approval、UserQuestion和provider I/O等待期间零持有上述短guard；
- SecurityRevoked和lifecycle/emergency signal不等待SessionExecutor acknowledgement或terminal cleanup；
- Rust实现启用Clippy`await_holding_lock`，并把Tokio Mutex/RwLock guard列入`await_holding_invalid_type`；typed permit的例外必须显式记录理由。

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
- 每个Session的`SessionFileMutationQueue`独立；同Session同canonical file mutation按call_index FIFO，不同file key可以并行；
- read-only文件Tool不进入queue；多文件与open-world Tool在同一批次按Serial执行；
- 不同Session对同一physical file可以并发mutation，MiniCore不承诺跨Session文件一致性；
- 一个Session的任一ingress/progress lane达到容量不能耗尽其他Session的lane容量；
- 跨Session仍可能在ModelGateway配额和共享宿主I/O处等待；
- Runtime shutdown逐Session设置PrepareForUnload；grace deadline到期后由各Session fail-closed cancel；
- UI current tab不影响后台Session执行。

特别是，Session A 的 UserQuestion waiter 和file mutation queue只属于 A 的`SessionExecutor`；A 在等待答案时，B/C/D仍可各自推进。共享ModelGateway配额或宿主I/O仍可能造成正常竞争，但不会因为UserQuestion或A的mutation queue把其他Session放入同一个等待队列。

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
    SubmitNotFound,
    NoRunningTurn,
    TurnCancelling,
    ExpectedTurnMismatch,
    InteractionNotFound,
    InteractionAlreadyResolved,
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
Lifecycle completion subscribers和SnapshotMailbox请求
AgentLoop internal state
Context/UserMessage composition/Model/Tool/Compaction async operation
approval waiter
provider continuation/session
process-local EmergencyControl signal/target generation
```

如果terminal entry已存在但projection仍有Pending Interaction或Started Item，load fail closed并要求explicit repair。

## Performance

MVP性能要求：

- 一个loaded Session一个Executor；
- TurnAdmission、Steer、FollowUp、InteractionControl和ToolControl lane分别有容量限制；
- EmergencyControl使用O(1) target/generation/sticky epoch且不保存Cancel completion sender；LifecycleControl使用shared completion generation；SnapshotMailbox只保留latest immutable view，不能无限增长；
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
- Submit在Executor仲裁时Session非Idle（Starting/Running/Finishing）立即返回SessionBusy，不跨Turn等待；
- terminal后decision窗口内到达的Submit参与该次公平admission；decision未选中立即SessionBusy；
- UI默认-Steer回退：Steer遇TurnNotRunning/ExpectedTurnMismatch后，同一输入改发Submit在Idle成功开启新Turn；
- duplicate in-flight Submit使用相同CommandId加入原completion，不创建第二个candidate；
- 相同CommandId携带不同command返回CommandConflict；
- CancelTarget::Submit(CommandId)在排队、Context capture和UserMessage composition期间取消candidate；target退休或restart后返回SubmitNotFound且不能影响future Turn；
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
- ModelGateway一次operation最多执行一个provider attempt，SDK automatic retry为0；
- AgentRun logical retry复用相同ModelCallRequest且不改变model identity，最多3次；initial加3次retry最多产生4次Gateway invocation；
- 第4个AgentRun retry不调度，logical retry计数在success或新ModelCallRequest时重置；
- CompactionSummary最多1次logical retry，即最多2次Gateway invocation；
- `NotSent`/`RejectedBeforeExecution`的Timeout/TransportUnavailable/ProviderUnavailable分别允许logical retry；AcceptedNoOutput默认不允许；
- `NotSent`/`RejectedBeforeExecution`的RateLimited在Retry-After不超过60秒时，delay取purpose backoff与hint较大值；超过60秒不自动等待；
- RequestOutcomeUnknown、StreamInterrupted、认证、quota、配置和安全错误不自动logical retry；
- UnexpectedToolCall、InvalidStructuredOutput、InvalidProviderResponse和IncompleteResponse不自动logical retry，不调用AgentLoop/ToolSet，不append assistant entry；
- non-empty Refused response仍进入candidate Finished并可truthful append；
- request delivery outcome unknown和first semantic delta后的stream failure不由Gateway blind retry；
- logical retry只在旧Model operation terminal/remove后启动；不存在两个可向Executor返回结果的同Session Model future；
- Finished candidate在Steer FIFO为空且Cancel/SecurityRevoked未获胜时才append final；
- Finished candidate遇到queued Steer时保存为model-visible Assistant Continue，再消费一条Steer；
- Steer admission与final commit reservation双向race：Steer先赢转Continue，reservation先赢拒绝Steer；
- NeedTools result无论Steer queue是否非空都先完成assistant/tool/tool_round_completed序列；
- streaming delta丢失不影响final entry；
- MVP不执行transport fallback；
- cross-model substitution返回typed failure而不是静默继续；
- logical retry_count准确记录0–3；Gateway没有transparent retry count。

### Tool

- Assistant(Intermediate)先于Tool执行；
- Cancel在ToolExecutionStarted前获胜时不执行副作用；
- ToolControlQueue已满时Cancel仍可设置EmergencyControl；
- record_execution_start处理前观察到Cancel/SecurityRevoked epoch时被拒绝；
- ToolExecutionStarted先获胜时Cancel不声称回滚；
- Cancel signal与ToolExecutionStarted controlled reservation双向race均有唯一结果，signal发布不因reservation阻塞；
- approval request append-before-notify；
- resolution append-before-resume；
- UserQuestion request在`ToolExecutionStarted`前append，等待期间不预留file mutation ticket；
- UserQuestion回答恢复同一个Tool future并形成`PreExecution` ToolResult，不伪造副作用；
- Presentation Adapter只消费UI-safe Interaction view并通过Runtime facade Resolve，不能直接写Interaction或持有waiter；
- WaitingForUserInput期间同Session不启动sibling副作用ToolCall，其他Session仍可运行；
- 长时间无回答保持Pending，不产生默认Deny；
- message/reasoning streaming start分配稳定ItemId，started/delta/completed identity一致；progress first-seen顺序不作为committed Item order；
- 丢失started或delta仍可由item_completed完整final view恢复；
- append失败不产生item_completed，Cancel/failure丢弃StreamingItem；logical retry通知Host清除上一operation临时view；
- item_completed或Turn terminal之后迟到的started/delta被Host忽略；
- CompactionSummary progress不创建StreamingItem、ItemId或item_completed；
- parallel ToolCall按source call order投影和展示，逆序完成只按ItemId更新原位置；
- 每个ToolResult独立append，物理result entry位置和Tool completion time都不作为Item sort key；
- active Items/Snapshot/turn-scoped ListItems使用同一canonical顺序，不公开DisplaySequence或ordinal；
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
- PrepareForUnload grace期仍能处理ResolveInteraction、Cancel、required ToolControl和operation result；
- unload deadline到期取消active Turn并以Cancelled关闭Pending Interaction。

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
- valid Cancel立即返回同一accepted epoch，不等待Tool settlement；
- Cancel(Submit)与initiating append reservation first-wins，accepted Cancel不创建Turn；
- Cancel(Turn)与final append reservation first-wins，accepted Cancel不提交Final Assistant；
- Finishing期间FollowUp可Queued，Steer拒绝且Submit SessionBusy；
- stale CancelTarget/generation不触发current或next Turn token；
- cancel epoch发布后新的同Turn Steer被拒绝，epoch前accepted Steer被清理；
- 普通lane全部满时Cancel仍立即发布sticky signal；
- Cancel不清空FollowUp或其他Submit；
- SecurityRevoked during Starting/Compacting/Sampling/WaitingApproval/WaitingForUserInput/ExecutingTools；
- SecurityRevoked while Idle closes admission, invalidates old Snapshot and resolves without creating TurnInterrupted；
- SecurityRevoked不等待任何bounded FIFO容量；
- stale target generation的signal不影响terminal后重新resolve并启动的future Turn；
- SecurityRevoked与Model start、ToolExecutionStarted、controlled append双向race遵循TurnControlGate first-wins；
- started Tool在security interruption下保存exact outcome或ToolAbandoned；
- terminal后Workspace重新resolve success Ready、failure Unavailable；
- open fd或已进入kernel/provider的operation不承诺动态撤销；
- Running failure关闭Pending/Started state；
- projection apply failure触发replay；
- replay failure使Session Unavailable。

### Multi-Session

- 两个Session同时Sampling；
- 一个Session WaitingApproval不阻塞另一个Session；
- 同Session同一canonical file的mutation按call_index FIFO；
- 同Session不同file key的mutation可以并行；
- read-only文件Tool不进入queue；任一Serial Tool使同批普通ToolCall按原始顺序执行；
- 两个Session对同一物理文件不共享queue，测试明确验证其无协调语义；
- 一个Session任一ingress lane满不消耗其他Session的lane容量；
- Session A ingress满时Session B的Cancel/Resolve/Submit仍可独立进入其自身lane；
- UI selection变化不影响后台Session；
- shutdown逐Session停止且不串写entry/event。

### Snapshot与Lane Arbitration

- snapshot-first subscription初始view capture与subscriber注册原子完成，后续事件无缺口且不回放旧事件；
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
EmergencyControl epoch与target generation
LifecycleControl state、grace deadline与completion subscriber count
Snapshot publication generation与subscriber count
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

否决原因：容易让独立operation task拥有AgentLoop、writer、queue或terminal状态，形成第二owner。自研AgentLoop是同步纯逻辑状态机，由主循环直接调用；原为Rig monolithic future保留的private adapter task例外已随ADR 0115删除。

### 每个Model/Tool/Interaction一个actor

否决原因：增加queue、shutdown、ordering和测试复杂度，但MiniCore每Session同时只有一个active Turn，没有对应收益。

### AgentLoop直接写SessionStorage

否决原因：Prompt、Tool、Interaction和Turn terminal的顺序会散落在SDK adapter内部，无法独立测试或替换AgentLoop。

### 一个Runtime只允许一个Running Session

否决原因：UI需要后台Session；SessionStorage、TurnExecutionContext和SessionIngress已经按Session隔离，共享Model使用明确配额，Workspace并发mutation由host/user通过worktree或独立Workspace协调。
