# Session Execution 架构设计

状态：当前权威架构（ADR 0137后；loaded Ready+Idle `SessionExecutor`、Runtime-owned residency actor、single-flight Load、draining Unload、lifecycle exclusion、unified loaded/unloaded Workspace update、Workspace Prompt candidate capture及replay/Recorder-backed hydration已实现；M7 ordinary Turn admission、immutable `TurnExecutionContext`、Input/final Assistant live apply与inline record、single scripted Model request、terminal Event和Unload/Load replay已接通public Runtime facade；M8.1最小Scripted Tool round-trip、M8.2 Interaction、M8.3 Cancel与M9.1–M9.14 crate-private queue/Steer/arbitration/retry/Snapshot/EmergencyControl seams已接通；具体Prompt/Skill source adapter、完整Tool policy/approval、完整public SecurityRevoked terminal route、public projections、Compaction及grace/cancel式active-Turn Unload pending）
日期：2026-07-31

## 目的

本文定义loaded Session的control actor、ActiveTurnTask、async run loop、SessionIngress、Steer/FollowUp、Cancel、Interaction routing、logical retry和restart行为。

当前实现进度：M9.14 已在 M9.13 Tool round 启动 safe-point 之后补齐 Steer 路径的 EmergencyControl basis。ActiveTurnTask 在 final/tool-round Steer 仲裁、`resolve_user_message()` await、live apply 以及下一次 Model 前持续确认同一 target+epoch 未 signal；Cancel/SecurityRevoked 或 stale epoch 先赢时丢弃迟到 candidate/Steer，不启动后续 Model。完整 public terminal/event route、public queue DTO/projection、retry progress 与 Compaction retry仍后置。

核心目标：

- 每个loaded Session最多一个active Turn task；
- Model、Tool、Interaction和retry用普通async控制流表达；
- control actor在Turn task await期间保持响应；
- live state驱动当前进程行为，Session recording outcome不作为correctness或retry permit；
- 多Session共享Runtime modules并可并发运行；
- restart不恢复旧task、future、waiter或queue。

## 决策摘要

- 一个loaded Session一个`SessionExecutor` control actor；
- 一个Running Session最多一个`ActiveTurnTask`；
- 删除同步`AgentLoop`、`next_action()`和`RunningOperation`；
- `ActiveTurnTask::run_turn`直接await ModelGateway、ToolSet和Interaction；
- `LiveSessionState`是current-process live truth；
- `SessionRecorder`顺序inline append有序best-effort前缀；
- complete Tool exchange由live conversation reducer判定；
- Starting capture/composition由control actor拥有的可取消future驱动，不创建第二个execution owner；
- Input与Steer共享TurnExecutionContext async `resolve_user_message()`，PromptSet normalize保持同步；
- Steer只在safe point消费；FollowUp只在旧task结束后启动；
- Cancel立即ack，Running Tool继续truthful settle；
- logical retry和backoff是task-local状态；
- recording Degraded或process crash时，Snapshot/StateEvent可以领先可恢复的recorded prefix；Healthy状态没有后台queue lag。

## Ownership

**Canonical cross-module invariant: INV-101.**

```text
MiniCoreRuntime
└─ LoadedSessionExecutors
   └─ SessionExecutor
      ├─ SessionIngress
      ├─ Arc<LiveSessionStateHandle>
      ├─ Arc<SessionRecorder>
      ├─ Arc<SessionFileMutationQueue>
      ├─ SessionSnapshot publisher
      ├─ FollowUpQueue
      └─ Option<ActiveTurnHandle>
         └─ ActiveTurnTask::run_turn
```

`SessionExecutor`拥有：

- Session load/readiness/lifecycle control；
- Submit和FollowUp admission；
- active task spawn、join和terminal handoff；
- Cancel、SecurityRevoked、Unload和Shutdown路由；
- Interaction resolution入口；
- snapshot-first subscription和StateEvent publication；
- recording health、current diagnostic和`session_recording_changed` publication；
- loaded Session-scoped `Arc<SessionFileMutationQueue>`与Turn-scoped Tool execution control handle构造。

`ActiveTurnTask`拥有：

- 当前Turn async control flow；
- exact `Arc<TurnExecutionContext>`；
- current phase和provisional streaming items；
- Model/Tool futures和ToolOperationSlot；
- logical retry counter/timer；
- current CompactionPlan与`compactions_started` bound；
- safe-point Steer消费；
- terminal candidate和settlement。

`LiveSessionState`保存当前进程的conversation、Turn、Item、Interaction、usage read model和private Session-scoped `EntryIdGenerator`。实现可以使用短锁或actor methods；guard不得跨await。SessionExecutor和ActiveTurnTask不能保存两份独立mutable conversation，也不能各自拥有ID generator。

每个Session最多一个ActiveTurnTask。同一个Runtime中的不同Session可以同时Running。

## 核心对象

```rust
pub(crate) struct SessionExecutor {
    session_id: SessionId,
    ingress: SessionIngress,
    live_state: LiveSessionStateHandle,
    recorder: Arc<SessionRecorder>,
    file_mutation_queue: Arc<SessionFileMutationQueue>,
    state: SessionExecutionState,
    active_turn: Option<ActiveTurnHandle>,
    follow_up: FollowUpQueue,
    published_snapshot: ArcSwap<SessionSnapshot>,
}
```

SessionExecutor在Starting candidate reservation时创建绑定`TurnId + control_generation + EmergencyControl`的Turn-scoped Tool execution control handle，并与`file_mutation_queue.clone()`一起进入`ToolTurnContext`。该handle先于ToolSet/TurnExecutionContext构造，随后由同一个ActiveTurnTask消费；不依赖task创建后的late injection。

```rust
pub(crate) struct ActiveTurnHandle {
    turn_id: TurnId,
    control: ActiveTurnControl,
    join: JoinHandle<TurnTaskOutcome>,
}
```

```rust
pub(crate) struct ActiveTurnControl {
    emergency: EmergencyControlHandle,
    steer_tx: mpsc::Sender<AcceptedSteer>,
    interaction_tx: mpsc::Sender<InteractionResolutionCommand>,
    lifecycle: LifecycleSignal,
}
```

不建立public `AgentLoop` trait、Turn runner registry或SDK-owned loop。

## SessionIngress

```text
TurnAdmissionQueue      Submit
FollowUpQueue           FollowUp
SteerControl            Steer(expected_turn_id)
InteractionControl      ResolveInteraction
EmergencyControl        Cancel / SecurityRevoked sticky epoch
LifecycleControl        PrepareForUnload / Shutdown
Snapshot                 immutable published view
```

规则：

- lane内部FIFO；
- 不建立跨lane全局FIFO；
- Emergency和Lifecycle不依赖普通bounded queue容量；
- stale TurnId/CommandId不能影响新task；
- Snapshot读取不等待Model/Tool future；
- control actor不得await整个Turn完成来处理Cancel或Interaction resolution。

## Durable gates, publication permit, and shutdown

No SessionExecutor caller holds an Agent or durable Session lifecycle/mutation gate while awaiting `DurableStateActor`; DurableStateActor alone takes private durable cross-entity gates in `Agent → Session` order and owns the mutation slot. Turn admission may hold `AgentAdmissionPermit` only through its no-I/O final Enabled check and Input live apply; it sends no DurableState request under that permit and releases it before `SessionRecorder.record().await`.

For a loaded Workspace definition update, SessionExecutor creates and owner-registers one `SessionDefinitionPublicationTask` with shared completion. That task—not the dispatch waiter—owns the distinct `SessionDefinitionPublicationPermit`, the prebuilt `WorkspaceSnapshot`, the DurableState actor request, and final infallible Snapshot installation. The permit excludes Starting/admission from the successful Idle check through durable publication and installation; DurableStateActor does not reacquire it. Caller/transport drop only drops a waiter and never cancels the owner task. Before `DurableCommitBarrier`, shutdown may settle the command as `Rejected(RuntimeClosing)` and release the permit; after the barrier, the task must settle durable publication plus Snapshot installation. Panic or invariant failure after possible commit resolves joined dispatch waiters with existing `RuntimeDispatchError::InternalDispatchUnavailable` and integrity-closes Runtime. This permit is process-local and is not a durable lifecycle gate.

当前crate-private foundation已经消费该合同：actor在spawn前安装publication slot/permit；changed Workspace先完成resolver candidate、Prompt source capture与authority/canonical resolution revalidation，再组装完整Snapshot并调用DurableState；Prompt source capture在closing先赢时取消，source unavailable/content rejection或revalidation mismatch均不发布candidate，旧Snapshot保持不变。Load在replay与Recorder准备完成后、executor安装前执行最后的authority/canonical及durable definition recheck，失败时关闭刚准备的Recorder且不安装residency。Skill source capture仍未实现，因此非空authorized Skill roots继续作为internal invariant fail closed。NoChange不调用resolver但仍经过authoritative durable stale/lifecycle recheck；actor在reap owner-tracked child后才安装new Snapshot、settle waiter并释放permit。request channel close会等待pre-close reserved permits和active publication；caller drop、close及post-commit-before-install barrier均由named deterministic tests覆盖；Load fault-and-replay conformance也以named barriers/faults证明admitted Load cancellation、replay worker spawn/panic/join failure、Recorder degraded initialization、stale Workspace/authority candidate拒绝和append后的cold replay。

`MiniCoreRuntime::shutdown().await` is the host-only, non-wire lifecycle seam; it does not add a fifth protocol entry point. It idempotently transitions Closing and rejects new admission, settles already accepted work by `EntityReservationBarrier`, `DurableCommitBarrier`, and `RecorderWriteBarrier`, stops/unloads SessionExecutors and joins their Recorder/publication jobs, joins DurableState staging/blocking jobs and stops the actor, closes conversation handles, releases the root lease, and marks Closed. Facade `Drop` sends only a best-effort Closing signal and never blocks; owner-registry self-Arcs keep tracked handles from detaching. Hosts must await shutdown before Tokio teardown to observe graceful completion and root-lease release.

## Session State

```rust
pub enum SessionExecutionState {
    Idle,
    Starting,
    Running,
    Finishing,
}
```

```rust
pub(crate) struct StartingCandidate {
    command_id: CommandId,
    turn_id: TurnId,
    control_generation: u64,
    observed_emergency_epoch: u64,
}
```

`StartingCandidate`只是control actor私有target/basis，不是领域Turn、后台task或durable record。candidate在任何async capture/composition前安装，使`Cancel(Submit(command_id))`覆盖整个Starting窗口。

- `Idle`：没有ActiveTurnTask，可admit Submit/FollowUp或更新Workspace definition；
- `Starting`：正在capture context、安装live Input、等待其record attempt或发布`TurnStarted`；
- `Running`：ActiveTurnTask存在；
- `Finishing`：task已停止新逻辑推进，正在settle Running Tool/Interaction并返回outcome。

```rust
pub enum TurnExecutionPhase {
    Sampling,
    ExecutingTools,
    WaitingApproval,
    WaitingForUserInput,
    RetryBackoff,
    Compacting,
}
```

phase只用于live Snapshot/Progress，不持久化。

## Turn Admission

```text
Submit accepted by control actor
→ reserve TurnId and control_generation
→ install StartingCandidate + Submit CommandId emergency target
→ create Turn-scoped Tool execution control handle
→ pin async capture TurnExecutionContext + resolve_user_message(intent) future
→ Starting subloop selects future / Cancel / SecurityRevoked / Lifecycle
→ future wins: revalidate candidate target + control_generation + emergency epoch + current authority
→ LiveSessionState validates/projects candidate + preflights ConversationRevision.checked_next()
→ allocates EntryId + binds parent_id
→ infallibly construct exact Arc<StoredSessionEntry>
→ infallibly bind prepared Input unit, commit live Input + Turn Running,
  append that same Arc to full selected path and install preflighted revision
→ await SessionRecorder.record(the same Arc)
→ publish TurnStarted
→ spawn ActiveTurnTask
→ complete original Submit as TurnStarted { turn_id }
```

control actor在Starting subloop中继续处理out-of-band Emergency/Lifecycle signals，不持有live-state、Agent/Session lifecycle、Workspace、SessionDefinitionPublicationPermit或publication guard跨await。user Cancel在Input apply前先赢时，actor drop capture/composition future、退休candidate，不创建领域Turn或task，并把原Submit完成为`SubmitCancelled`；SecurityRevoked/Lifecycle/Runtime shutdown使用对应typed rejection。

resolve future返回不授予apply权。actor必须重新确认same `command_id + turn_id + control_generation`、observed emergency epoch仍current、Agent/Session仍可执行且Workspace authority未被hard-revoke；任一不匹配直接丢弃结果。SkillService shared cache parse可以继续，但不能反向发布UserMessage。

Submit在当前inline record attempt返回后发布`TurnStarted`并响应。Recorder已经Degraded时`record()`立即返回`NotRecorded`，Turn仍可开始；Snapshot必须暴露相应recording health。Input live apply一旦成功，Runtime shutdown不能再把原Submit改成`Rejected(RuntimeClosing)`：若shutdown在physical `RecorderWriteBarrier`前获胜，Recorder返回`NotRecorded`并进入/保持Degraded，owner仍发布/完成`TurnStarted`，随后把shutdown/cancel绑定到同一Turn并settle interruption/close。

`Starting`期间`Cancel(Submit(command_id))`持续有效，包括Input已经live apply但`TurnStarted`尚未发布的窗口。Input apply前user Cancel使原Submit完成`SubmitCancelled`；Input已apply时Cancel绑定已经分配的同一TurnId、发布sticky epoch并阻止ActiveTurnTask spawn，Input record attempt返回后仍先发布/完成`TurnStarted`，随后完成live `TurnInterrupted(UserCancelled)` settlement。调用方收到`TurnStarted`后改用`Cancel(TurnId)`。

capture、Workspace、Prompt composition或live validation失败时不创建Turn。Prompt composition必须先按[INV-202](../architecture.md#跨模块不变量索引)验证全部ordered Skill/Workspace contributions，失败时不apply部分Input。encode/write失败发生在live apply之后，只降低recording health，不把Submit改成失败。

## Async Run Loop

概念实现：

```rust
async fn run_turn(mut turn: ActiveTurnExecution) -> TurnTaskOutcome {
    loop {
        turn.check_emergency()?;
        turn.consume_one_safe_point_steer().await?;

        let views = turn.live_state.capture_conversation_views()?;
        let request = turn.build_agent_run_request(views.conversation())?;
        let response = turn.call_model_with_logical_retry(request).await?;

        if response.has_tool_calls() {
            turn.apply_assistant_tool_calls(response).await?;
            turn.execute_tool_exchange().await?;
            continue;
        }

        match turn.arbitrate_candidate_or_steer(response).await? {
            CandidateDecision::Continue => continue,
            CandidateDecision::Finish(candidate) => {
                return turn.finish_completed(candidate).await;
            }
        }
    }
}
```

`build_agent_run_request()`只是ActiveTurnTask private convenience：它调用同一`TurnExecutionContext::assemble_agent_run(&conversation)`，随后使用ModelGateway唯一拥有的constructor传入exact `Arc<TurnModelSnapshot> + ModelCallPurpose::AgentRun + Arc<AssembledModelContext> + conversation.revision + None`。它不定义第二个request DTO，也不省略source revision/output-limit语义。

这里的`await`不表示ActiveTurnTask可以任意写Session。所有live mutation仍通过`LiveSessionState` private typed methods；所有recording只通过`SessionRecorder`。调用`record().await`前必须释放live-state guard。

## Live Mutation与Recording

每个recordable conversation mutation使用同一局部顺序：

```text
validate Turn/control_generation/conversation_revision and prepare all body projections/candidates
→ determine revision delta; when +1, ConversationRevision.checked_next()
→ LiveSessionState.EntryIdGenerator.allocate()? and bind parent_id
→ infallibly construct exact Arc<StoredSessionEntry>
→ infallibly bind any prepared new-origin stable unit and commit prepared LiveSessionMutation
→ append that same Arc to full selected path and install preflighted revision
→ await recorder.record(the same Arc)
→ publish final StateEvent or continue protocol
```

all normal-result fallible validation/projection/candidate preparation finishes before allocation. revision overflow and typed EntryId allocation failure both return before any state/head mutation; after allocation exact entry-Arc construction, prepared-unit binding, commit, same-Arc path append and revision installation are infallible (ordinary allocation panic is outside Result), so a returned error never consumes an ID and state never applies before its entry Arc exists. allocation is CSPRNG-backed, reserves a unique candidate before return with at most 32 collision attempts, never a panic/fatal path.

`record().await`是inline non-durable append attempt。`Written`不表示flush或fsync；encode或writer error with the root lease intact makes Recorder Degraded, while root lease/lock-file identity loss is global poison/close. Recorder owns/registers one in-flight blocking job before spawn; `record().await` and panic/unload finalization await its shared settlement after releasing any raw guard. mutation不回滚，Turn不失败。TurnStatus mutation不属于recordable conversation：Completed与Final Assistant在同一live decision中形成，Interrupted/Failed只发布live terminal StateEvent。

禁止：

- record成功前暂存另一份“pending committed conversation”；
- recording failure后重新执行Model或Tool；
- 让SessionRecorder创建、替换或规范化EntryId；
- 从JSONL line number、storage ordinal或ConversationRevision派生EntryId；
- 从cold projector取得live推进permit；
- 让UI event反向修改live state。

## Model

ActiveTurnTask调用：

```rust
ModelGateway::generate_model_turn(
    Arc<ModelCallRequest>,
    CancellationToken,
) -> Result<ModelCallResult, ModelCallError>
```

ModelGateway仍只执行一个provider attempt。validated final response返回task；streaming delta只更新provisional Item和ProgressEvent。

Assistant final/tool-call response先apply live并完成inline record attempt，再产生final Item StateEvent或启动Tool。Provider error不创建assistant Item。

## Logical Retry

logical retry由ActiveTurnTask拥有：

```text
retryable ModelCallError
→ verify Turn still Running
→ verify same control_generation
→ verify same ConversationRevision
→ retain exact same Arc<ModelCallRequest> produced by the ModelGateway-owned constructor
→ phase = RetryBackoff
→ select(cancel, security, lifecycle, sleep(delay))
→ next ModelGateway attempt
```

AgentRun默认最多3次logical retry；CompactionSummary默认最多1次，错误分类和delay继续遵守ADR 0119。

ActiveTurnTask不读取或复制request内部字段来建立第二份retry proof；request identity、control generation和ConversationRevision共同构成当前process内的重验basis。

Steer在RetryBackoff期间只排队，不改变conversation revision；Cancel/SecurityRevoked立即使retry失效。旧provider future的结果路径必须在开始下一attempt前关闭。

## Tool Execution

```text
validated assistant ToolCall content
→ normalize Tools-owned ToolCalls + allocate ItemIds + build ToolExecutionRequests
→ owner-local apply Assistant(intermediate) + Started Items + expected call set
→ install matching Prepared ToolOperationSlots in the same no-await step
→ await inline record assistant attempt
→ phase ExecutingTools
→ execute calls through captured ToolSet
→ each truthful result applies live + await inline record attempt
→ live reducer checks complete exchange
→ complete: consume safe-point Steer or next Model
```

Tool futures可以并行，但属于同一个ActiveTurnTask。它们不能直接修改LiveSessionState或发布final StateEvent，只返回typed outcomes给task。accepted ToolCall Assistant即使仍被complete gate隐藏也使`ConversationRevision +1`；最后一个matching first truthful `Completed` result promotion完整exchange时再`+1`，partial/abandoned settlement保持`+0`。

### Tool Operation Slot

```rust
pub(crate) enum ToolOperationSlot {
    Prepared { request: ToolExecutionRequest },
    Running {
        request: ToolExecutionRequest,
        cancellation: ToolCancellationHandle,
    },
    Settling { request: ToolExecutionRequest },
    Terminal { outcome: ToolExecutionOutcome },
}
```

Session Execution是`ToolOperationSlot`的唯一type owner。Turn/Item只投影`Started | Completed | Abandoned`，Tools通过Turn-scoped `ToolExecutionControl`请求`Prepared → Running` first-wins reservation；ActiveTurnTask随后推进`Settling → Terminal`。pre-execution exact outcome允许`Prepared → Terminal(Completed)`，只有possible-start且outcome unknown才能进入`Terminal(Abandoned)`。Tool start遵守INV-401，recording不是Tool start marker。

## Interaction

ActiveTurnTask通过SessionExecutor-owned Interaction router等待approval或UserQuestion：

```text
task sends InteractionRequestCommand
→ actor validates active Turn/Item
→ apply Pending Interaction live
→ await inline record request attempt
→ publish StateEvent
→ task awaits oneshot
→ actor receives ResolveInteraction
→ validate family/key/current Turn
→ apply resolution live
→ await inline record resolution attempt
→ complete oneshot
```

等待期间不持有live state lock、file mutation permit或ToolStartGate。每Item至多一个Pending Interaction；terminal resolution后允许顺序later interaction。request/resolution、失败和idempotent resolution都model-invisible且revision `+0`；recording degraded时Interaction仍正常运行。

## Steer

**Canonical cross-module invariant: INV-102.**

```text
Steer(expected_turn_id, command_id)
→ SessionExecutor validates active task
→ enqueue to ActiveTurnControl steer channel
→ return Queued
```

ActiveTurnTask只在以下safe point消费一条FIFO Steer：

- complete Tool exchange已经进入live conversation后；
- 无ToolCall candidate被保存为Assistant Continue后；
- Compaction Replace完成后；
- 下一次Model assembly前。

```text
pop one Steer
→ capture active TurnId/control_generation/ConversationRevision basis
→ select Emergency/Lifecycle vs context.resolve_user_message(intent)
→ resolve wins: revalidate same basis and captured Context
→ composition失败时不apply部分Steer
→ success才apply live UserMessage(source=Steer)
→ await inline record attempt
→ next Model
```

WaitingApproval、WaitingForUserInput、Sampling和Running Tool期间不消费。

Steer resolve期间control actor仍可接收后续Steer/FollowUp/Interaction与sticky emergency signal。shared/Workspace reload不替换active Context；successful resolve只使用old captured Skill bytes。Cancel/SecurityRevoked先赢或basis改变时drop result，不apply部分Steer，也不把该intent重新排到队尾。

candidate final与Steer admission通过control actor的owner-local arbitration排序。该线性化点是内存control decision，不是Session record。

## FollowUp

FollowUp由SessionExecutor保存，Running/Finishing期间可以排队。ActiveTurnTask返回terminal outcome后：

```text
actor clears active task
→ state Idle
→ apply/publish live terminal if task尚未完成
→ admission arbitration
→ optional FollowUp starts new Turn with new context
```

FollowUp不复用旧TurnExecutionContext、Tool state、retry state或conversation revision basis。

## Cancel

```text
Cancel(target)
→ validate active command/Turn
→ publish sticky cancel epoch
→ signal candidate / ActiveTurnTask / Tool cancellation handles
→ return CancelAccepted
```

Starting candidate尚未spawn ActiveTurnTask时，accepted Cancel阻止spawn，并在已经live apply的Input完成record attempt与`TurnStarted` publication后直接进入live interruption settlement。ActiveTurnTask已经存在时随后：

- 停止新Model、Tool、Compaction和Steer消费；
- Prepared Tool不启动；
- Running Toolbest-effort cancel并等待truthful outcome或Abandoned；
- Pending Interaction在live state中Cancelled并resume waiter；
- apply live TurnInterrupted(UserCancelled)；
- publish TurnInterrupted StateEvent；
- return Interrupted outcome。

Cancel response不等待recording或settlement。

若Cancel在Starting capture/composition await期间、Input apply前先赢，control actor立即drop该future并退休candidate；Skill load/cache的迟到结果没有apply capability。若Input已apply，则继续使用上一段的`TurnStarted`后interruption顺序。

## SecurityRevoked

SecurityRevoked使用同一EmergencyControl机制，terminal reason不同。Starting capture/composition await阶段在Input live apply前取消candidate、drop future且不创建Turn；Input已apply时绑定同一Turn、阻止task spawn，并在Input publication后完成live `TurnInterrupted(SecurityRevoked)`。task结束后SessionExecutor使用current definition/authority重新resolveWorkspace；失败时进入`SessionReadiness::Unavailable(WorkspaceUnavailable)`。

recording failure不会产生Unavailable。

## M10 Compaction

M4只实现source/cut/marker/no-I/O的live-reducer subset：apply还接收Session/Turn orchestration提供的`TurnId + Timestamp`。它创建fresh current source，`CompactionReplacement`必须匹配该source derived marker，且pending/cross-session/stale或`source.has_same_stable_identity(&fresh_current_source)`不匹配时在EntryId allocation前拒绝；it consumes exact replacement parts and prepares the rolling-summary stable unit before allocation, then retains suffix only from fresh current source. M4 replacement construction is only `#[cfg(test)] for_m4_test(...) -> Result<_, CompactionReplacementError>`; M10 will add production construction from `ValidatedCompactionSummary` when those types exist. M4不构造plan/request、不验证model provenance、不调用model/Recorder或发布。M5负责recorded bad-marker ignore/diagnose。以下是M10 full Compaction flow：

```text
exact next AgentRun pressure/Prompt ContextOverflow/provider ContextOverflow
→ short live-state capture:
     CapturedConversationViews at one revision
     (LiveConversationView + Arc<LiveCompactionSourceView> + private read facts)
→ release live-state guard
→ context.compaction_pressure(source, trigger, compactions_started)
→ context.plan_compaction(exact source, trigger, compactions_started)
→ assemble CompactionSummary + Arc<ModelCallRequest>
→ atomically install exact Arc<CompactionPlan> + Arc<ModelCallRequest>
   as current task-local operation and increment compactions_started
→ phase Compacting
→ call ModelGateway with at most one logical retry using same request
→ verify exact Turn/control/session/revision/plan/request and no winning emergency
→ Compaction validates summary + complete automatic provenance
→ M10 constructs CompactionReplacement from ValidatedCompactionSummary
→ final no-await verify exact Turn/control/session/revision/plan/request and no winning emergency
→ one no-await live-owner operation:
     consume exact replacement parts; prepare rolling-summary stable unit
     create fresh current source; validate `source.has_same_stable_identity(&fresh_current_source)` + derive marker from plan cut
     prepare exact current-suffix Replace + ConversationRevision.checked_next()
     allocate Compaction EntryId + bind parent
     infallibly construct exact entry Arc, bind new rolling-summary origin,
       commit Replace with exact retained units, append same Arc to full path,
       and install preflighted revision
     return exact AppliedConversationFact record candidate
→ await inline SessionRecorder.record(the same Arc) attempt
→ consume one safe-point Steer or reassemble AgentRun
```

`compactions_started`计算installed summary logical call chains；logical retry不再次增加，pressure/plan失败不增加。Recommended plan不可行但ordinary AgentRun仍通过Prompt validation时，task可以保留该AgentRun并发布diagnostic；Required plan不可行时按ContextOverflow failure收口。

queued Steer在Compaction期间只排队，不改变revision或使request失效；safe point实际apply后才递增revision。Cancel/SecurityRevoked可以在revision未变时拒绝summary result。record marker丢失时current process继续使用summary；restart恢复旧conversation。Compaction不重建AgentLoop，也不让control actor await整个operation。

## Terminal与Failure

Turn terminal只属于current-process live state：

```text
Completed
→ apply Final Assistant + TurnCompleted live
→ await inline record Final Assistant attempt
→ publish ItemCompleted + TurnCompleted

Interrupted / Failed
→ finish recordable Tool/Interaction settlement facts
→ apply live TurnInterrupted / TurnFailed
→ publish terminal StateEvent
→ task returns TurnTaskOutcome
```

Model/Prompt/Tool invariant failure可以使Turn Failed/Interrupted，但不会创建synthetic Session entry。`TurnStarted`完成后发生的Prompt/Model/Tool/Compaction failure只通过Turn terminal StateEvent/Snapshot表达，不能retroactively Reject原Submit。pre-Turn composition/admission failure才映射为Command Rejected；user Cancel特例映射为SubmitCancelled。Session recording first failure原子更新internal `RecordingHealth`、公开`SessionRecordingState::Degraded`和当前脱敏diagnostic；Turn继续。触发该record attempt的conversation StateEvent先携带Degraded Snapshot发布，随后补发一次`session_recording_changed`。后续`NotRecorded`不重复发布state event。

如果ActiveTurnTask panic或channel异常退出，SessionExecutor把live Turn标记`Interrupted(RuntimeFailure)`，发布terminal StateEvent并进入Idle或按Workspace readiness进入Unavailable。JSONL不追加RuntimeFailure terminal。

## Snapshot与Events

SessionExecutor发布的Snapshot包含：

- SessionExecutionState和optional current Turn；
- ActiveTurn phase；
- live Items和Pending Interaction；
- exact Submit admission/Steer/FollowUp queue views（CommandId与lane-local FIFO，不含prompt body/preview）；
- usage；
- recording health；
- redacted diagnostics。

SessionExecutor在一次短owner-state capture中同时投影execution/current Turn与三个public input lane。QueueView不得从event history重建，也不得只读atomic count后再分批读取IDs；否则snapshot可能出现count与cancel target不一致。该capture不等待Model/Tool/Recorder I/O。

recording Degraded或process crash时，Snapshot/StateEvent可能领先可恢复JSONL prefix。Host不能把收到`item_completed`或`turn_completed`解释为flush/fsync acknowledgement。

## Recovery

```text
open DurableState PublishedConversationTarget with optional root-lease-derived writable proof
→ tolerant replay complete lines
→ writable Load truncates only final unterminated partial tail
→ construct LiveSessionState from recorded prefix
→ sanitize incomplete Tool exchanges
→ initialize new inline SessionRecorder at replayed recorded head
→ capture current Workspace/readiness
→ current_turn = None
→ start SessionExecutor Idle or Unavailable
```

不恢复：

- ActiveTurnTask；
- ModelCallRequest/provider stream；
- Tool task/process handle；
- Interaction waiter；
- Submit admission/Steer/FollowUp queue；
- retry timer；
- old Recorder object或in-flight append。

当前Ready+Idle hydration把replay-seeded `LiveSessionState`和同一target parts初始化的`SessionRecorder`一并交给`SessionExecutor`；actor close先关闭/等待Recorder，再完成residency Unload。cold seed保留selected path、stable units、relations、replay revision与reserved EntryId，但按recovery contract保持`current_turn = None`并不恢复Interaction waiter。

Load不推断旧Turn outcome、不恢复terminal reason，也不执行recovery append。recorded `TurnId`只用于conversation history grouping。当前loaded instance一旦Degraded便保持Degraded，不probe/retry、不创建segment、不backfill。Host执行Unload/Load后，新loaded instance只从recorded prefix开始并重新尝试初始化Recorder，结果为Healthy或Degraded；旧unrecorded live tail和旧TurnStatus永久丢失。

## Tokio运行时、blocking work与deterministic seams

This module consumes [ADR 0137](../adr/0137-tokio-owner-tracked-async-foundation.md) and [DurableState](durable-state.md); it does not restate their dependency-version, filesystem-adapter, or physical-publication taxonomy. Host construction explicitly injects one cloned Tokio `Handle` into MiniCore; Session execution receives only the internal verified `RuntimeTaskContext`, never `Handle::current()` or other ambient context, and MiniCore does not construct/nest a runtime. The Session-specific obligations are: ordinary guards never cross await; `RuntimeTaskContext` owns/tracks/joins each Recorder and SessionDefinitionPublication job; caller drop never detaches a started append/publication; panic/unload finalizers await shared settlement before releasing the loaded owner; and no persistent worker/background queue exists.

Cancellation only wins before the named barriers. A `RecorderWriteBarrier` crossing makes append settlement mandatory; `DurableCommitBarrier` is owned by DurableState. `CancellationToken` wakes work but owner state/epoch determines first-wins. Injectable clocks, named fault coordinates, and joins—not Tokio time advance—prove settlement. Raw guard linting and the narrow documented typed-permit exceptions are governed by ADR 0137.

## 测试要求

- 每Session最多一个ActiveTurnTask；
- 多Session并发Model calls；
- ordinary Model→Tool→Model async flow；
- 所有recordable mutation由同一LiveSessionState generator分配EntryId；
- Degraded继续分配ID，Recorder看到的ID与live Item/Interaction/Compaction一致；
- parallel Tool completion与ordered complete exchange；
- ToolSet在ActiveTurnTask spawn前可用exact control handle和SessionFileMutationQueue构造；
- Prepared slot与Cancel/SecurityRevoked first-wins，cancel-before-start产出matching ToolResult；
- 同一Session same-file FIFO、different-file parallel，跨Session同physical target不协调；
- Steer safe-point和final arbitration；
- Interaction oneshot与Cancel；
- Starting async Skill load期间user Cancel先赢时原Submit为SubmitCancelled且无live Input/Turn/task；SecurityRevoked/Lifecycle使用typed rejection；
- resolve返回后candidate/control/emergency/authority任一stale都丢弃结果；
- Input已apply后的Starting Cancel仍先完成TurnStarted再Interrupted；
- Steer resolve期间reload不改变captured bytes，Cancel/SecurityRevoked或revision变化使结果失效；
- composition await不持有live/lifecycle/Workspace guard；
- retry backoff被Cancel/SecurityRevoked打断；
- slow append只延迟同Session finalization，其他Session与sticky EmergencyControl仍可推进；
- recording failure后Snapshot/StateEvent仍反映live state；
- Degraded后修复storage不会使当前loaded instance重新写入；
- Unload/Load可建立新Healthy Recorder，但只恢复recorded prefix；
- `capture_conversation_views()`将Compaction source、ordinary LiveConversationView、从full selected `Arc<StoredSessionEntry>` path派生的head、relations与safe Pending facts在同一revision aggregate产生；M4只读head，future LiveSnapshot/Fork保留full path，guard在planning/await前释放；
- duplicate message和complete Tool exchange只能在stable-unit boundary派生marker；
- Compaction settings、Prompt bases和model basis来自同一TurnExecutionContext；
- Compaction budget等于Prompt proof和ModelCallRequest max output；
- `compactions_started`计logical call chain、不计retry，达到默认4次后hard overflow fail closed；
- queued Steer不使Compaction stale，consumed Steer与Cancel/SecurityRevoked按各自basis拒绝result；
- live Compaction EntryId allocation/Replace先于recording，record failure保留summary；
- Snapshot完整枚举Submit admission、Steer和FollowUp cancel targets，lane-local FIFO稳定且不泄漏queued intent正文；
- QueueView与execution/current Turn来自同一次owner snapshot，Starting→TurnStarted切换不会同时暴露旧Submit target和new Turn target；
- task panic收口；
- restart只恢复recorded conversation prefix，current_turn为空且无synthetic terminal。

## 开放问题

RecordingHealth wire形状已由Q2关闭，Degraded recovery已由Q5关闭，所有Session强制尝试记录且无Disabled policy已由Q7关闭，EntryId owner已由Q9关闭，Turn lifecycle omission与无closure Load已由Q10/ADR 0127关闭。Recorder问题Q1–Q10均已关闭，结论见[Async Loop与Best-Effort Session Recording开放问题](../review/async-loop-best-effort-recording-open-questions.md)。
