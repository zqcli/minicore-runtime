# Session Execution 架构设计

状态：当前权威架构（ADR 0130后，生产实现待启动）
日期：2026-07-31

## 目的

本文定义loaded Session的control actor、ActiveTurnTask、async run loop、SessionIngress、Steer/FollowUp、Cancel、Interaction routing、logical retry和restart行为。

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
→ LiveSessionState validates + allocates EntryId + binds parent_id
→ apply live Input + Turn Running
→ await SessionRecorder.record(entry)
→ publish TurnStarted
→ spawn ActiveTurnTask
→ return SubmitAccepted { turn_id }
```

control actor在Starting subloop中继续处理out-of-band Emergency/Lifecycle signals，不持有live-state、Agent/Session lifecycle、Workspace或publication guard跨await。Cancel/SecurityRevoked在Input apply前先赢时，actor drop capture/composition future、退休candidate并完成原Submit内部cancel/revoked settlement；不创建领域Turn，也不spawn ActiveTurnTask。其public typed response映射由Runtime protocol freeze单独定义。

resolve future返回不授予apply权。actor必须重新确认same `command_id + turn_id + control_generation`、observed emergency epoch仍current、Agent/Session仍可执行且Workspace authority未被hard-revoke；任一不匹配直接丢弃结果。SkillService shared cache parse可以继续，但不能反向发布UserMessage。

Submit在当前inline record attempt返回后发布`TurnStarted`并响应。Recorder已经Degraded时`record()`立即返回`NotRecorded`，Turn仍可开始；Snapshot必须暴露相应recording health。

`Starting`期间`Cancel(Submit(command_id))`持续有效，包括Input已经live apply但`TurnStarted`尚未发布的窗口。此时Cancel绑定已经分配的同一TurnId、发布sticky epoch并阻止ActiveTurnTask spawn；Input record attempt返回后仍先发布`TurnStarted`，随后完成live `TurnInterrupted(UserCancelled)` settlement。调用方收到`TurnStarted`后改用`Cancel(TurnId)`。

capture、Workspace、Prompt composition或live validation失败时不创建Turn。Prompt composition必须先按[INV-202](../architecture.md#跨模块不变量索引)验证全部ordered Skill/Workspace contributions，失败时不apply部分Input。encode/write失败发生在live apply之后，只降低recording health，不把Submit改成失败。

## Async Run Loop

概念实现：

```rust
async fn run_turn(mut turn: ActiveTurnExecution) -> TurnTaskOutcome {
    loop {
        turn.check_emergency()?;
        turn.consume_one_safe_point_steer().await?;

        let context = turn.prompt_set.assemble(
            turn.live_conversation.sanitized_view(),
            ModelCallPurpose::AgentRun,
        )?;
        let request = Arc::new(ModelCallRequest::new(
            turn.model.clone(),
            context,
        )?);

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

这里的`await`不表示ActiveTurnTask可以任意写Session。所有live mutation仍通过`LiveSessionState` private typed methods；所有recording只通过`SessionRecorder`。调用`record().await`前必须释放live-state guard。

## Live Mutation与Recording

每个recordable conversation mutation使用同一局部顺序：

```text
validate Turn/control_generation/conversation_revision
→ LiveSessionState allocates EntryId and binds parent_id
→ apply LiveSessionMutation
→ await recorder.record(StoredSessionEntry)
→ publish final StateEvent or continue protocol
```

`record().await`是inline non-durable append attempt。`Written`不表示flush或fsync；encode或writer error使Recorder Degraded。mutation不回滚，Turn不失败。TurnStatus mutation不属于recordable conversation：Completed与Final Assistant在同一live decision中形成，Interrupted/Failed只发布live terminal StateEvent。

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

Tool futures可以并行，但属于同一个ActiveTurnTask。它们不能直接修改LiveSessionState或发布final StateEvent，只返回typed outcomes给task。

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

等待期间不持有live state lock、file mutation permit或ToolStartGate。recording degraded时Interaction仍正常运行。

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

## Compaction

```text
exact next AgentRun pressure/Prompt ContextOverflow/provider ContextOverflow
→ short live-state capture:
     LiveConversationView + Arc<LiveCompactionSourceView> at one revision
→ release live-state guard
→ context.compaction_pressure(source, trigger, compactions_started)
→ context.plan_compaction(exact source, trigger, compactions_started)
→ install exact Arc<CompactionPlan> as current task-local operation
→ assemble CompactionSummary + Arc<ModelCallRequest>
→ increment compactions_started before first Gateway call
→ phase Compacting
→ call ModelGateway with at most one logical retry using same request
→ verify exact Turn/control/session/revision/plan/request and no winning emergency
→ Compaction validates summary + complete automatic provenance
→ one no-await live-owner operation:
     derive marker from plan cut
     allocate Compaction EntryId + bind parent
     Replace with new rolling-summary origin + exact retained units
     increment ConversationRevision once
     return same StoredSessionEntry record candidate
→ await inline SessionRecorder.record attempt
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

Model/Prompt/Tool invariant failure可以使Turn Failed/Interrupted，但不会创建synthetic Session entry。Session recording first failure原子更新internal `RecordingHealth`、公开`SessionRecordingState::Degraded`和当前脱敏diagnostic；Turn继续。触发该record attempt的conversation StateEvent先携带Degraded Snapshot发布，随后补发一次`session_recording_changed`。后续`NotRecorded`不重复发布state event。

如果ActiveTurnTask panic或channel异常退出，SessionExecutor把live Turn标记`Interrupted(RuntimeFailure)`，发布terminal StateEvent并进入Idle或按Workspace readiness进入Unavailable。JSONL不追加RuntimeFailure terminal。

## Snapshot与Events

SessionExecutor发布的Snapshot包含：

- SessionExecutionState和optional current Turn；
- ActiveTurn phase；
- live Items和Pending Interaction；
- Steer/FollowUp queue summary；
- usage；
- recording health；
- redacted diagnostics。

recording Degraded或process crash时，Snapshot/StateEvent可能领先可恢复JSONL prefix。Host不能把收到`item_completed`或`turn_completed`解释为flush/fsync acknowledgement。

## Recovery

```text
open recorded Session and attempt writable lease
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
- Steer/FollowUp queue；
- retry timer；
- old Recorder object或in-flight append。

Load不推断旧Turn outcome、不恢复terminal reason，也不执行recovery append。recorded `TurnId`只用于conversation history grouping。当前loaded instance一旦Degraded便保持Degraded，不probe/retry、不创建segment、不backfill。Host执行Unload/Load后，新loaded instance只从recorded prefix开始并重新尝试初始化Recorder，结果为Healthy或Degraded；旧unrecorded live tail和旧TurnStatus永久丢失。

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
- Starting async Skill load期间Cancel/SecurityRevoked先赢时无live Input、无Turn、无task spawn；
- resolve返回后candidate/control/emergency/authority任一stale都丢弃结果；
- Input已apply后的Starting Cancel仍先发布TurnStarted再Interrupted；
- Steer resolve期间reload不改变captured bytes，Cancel/SecurityRevoked或revision变化使结果失效；
- composition await不持有live/lifecycle/Workspace guard；
- retry backoff被Cancel/SecurityRevoked打断；
- slow append只延迟同Session finalization，其他Session与sticky EmergencyControl仍可推进；
- recording failure后Snapshot/StateEvent仍反映live state；
- Degraded后修复storage不会使当前loaded instance重新写入；
- Unload/Load可建立新Healthy Recorder，但只恢复recorded prefix；
- Compaction source与ordinary LiveConversationView在同一revision snapshot产生，guard在planning/await前释放；
- duplicate message和complete Tool exchange只能在stable-unit boundary派生marker；
- Compaction settings、Prompt bases和model basis来自同一TurnExecutionContext；
- Compaction budget等于Prompt proof和ModelCallRequest max output；
- `compactions_started`计logical call chain、不计retry，达到默认4次后hard overflow fail closed；
- queued Steer不使Compaction stale，consumed Steer与Cancel/SecurityRevoked按各自basis拒绝result；
- live Compaction EntryId allocation/Replace先于recording，record failure保留summary；
- task panic收口；
- restart只恢复recorded conversation prefix，current_turn为空且无synthetic terminal。

## 开放问题

RecordingHealth wire形状已由Q2关闭，Degraded recovery已由Q5关闭，所有Session强制尝试记录且无Disabled policy已由Q7关闭，EntryId owner已由Q9关闭，Turn lifecycle omission与无closure Load已由Q10/ADR 0127关闭。Recorder问题Q1–Q10均已关闭，结论见[Async Loop与Best-Effort Session Recording开放问题](../review/async-loop-best-effort-recording-open-questions.md)。
