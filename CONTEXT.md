# MiniCore Agent Runtime

本上下文描述MiniCore V2当前架构。ADR 0126已经把Turn执行重构为async loop，并把Session持久化降级为inline best-effort recording。

权威顺序：`docs/architecture.md`与`docs/modules/` → Accepted ADR → `docs/research/` → `docs/archive/v1/`。

## 核心术语

**MiniCore**：
可嵌入CLI、TUI、GUI可信宿主的原生Agent harness runtime core。负责Session、async Turn执行、Prompt、Tool、Skill、ModelGateway、Runtime协议、观察事件和best-effort recording。

**MiniCoreRuntime**：
下游host唯一顶层门面，通过`dispatch / query / snapshot / subscribe`提供能力。内部拥有PromptService、ToolService、SkillService、ModelGateway和LoadedSessionExecutors，不保存UI selected Session。

**Runtime Interface**：
由RuntimeCommand/CommandResponse、RuntimeQuery/QueryResponse、RuntimeSnapshot/SessionSnapshot和StateEvent/ProgressEvent组成的transport-neutral interface。

**Runtime-owned共享module**：
`PromptService`、`ToolService`、`SkillService`和`ModelGateway`。四个current immutable resource roots只在initialize或显式reload成功后整体publication。

**Agent**：
可被多个Session引用的durable definition owner。AgentRevision immutable；Session pin exact AgentRevisionRef，不自动跟随Agent current。

**Session**：
长期工作上下文。SessionDefinition绑定AgentRevisionRef、Workspace、Model config和Prompt selection。Conversation recording失败或process crash时可以缺少loaded live tail。

**SessionExecutor**：
每个loaded Session一个的control actor。拥有SessionIngress、lifecycle、Submit/FollowUp admission、active-task handle、Snapshot publisher和SessionRecorder handle。它不再拥有同步AgentLoop或RunningOperation。

**SessionExecutionHandle**：
Runtime内部路由到SessionExecutor的cloneable handle。下游host不能取得该handle。

**ActiveTurnTask**：
每个Running Session最多一个的async task。直接await ModelGateway、ToolSet、Interaction resolution、retry timer和Compaction，并拥有current Turn的异步控制流。

**ActiveTurnControl**：
SessionExecutor向ActiveTurnTask发送Steer、Interaction resolution、Cancel、SecurityRevoked和Lifecycle signal的crate-private channels/tokens。

**LiveSessionState**：
loaded Session的current-process truth，保存live conversation、Turn、Item、Interaction、usage和read model。它通过private typed methods修改；任何lock guard不得跨await。

**LiveConversation**：
模型协议安全的current-process conversation reducer。它拥有expected ToolCalls、first terminal result、complete exchange、Compaction Replace和ConversationRevision。

**LiveConversationView**：
PromptSet可消费的sanitized只读view。只包含provider-valid messages；incomplete、orphan或abandoned-first Tool exchange被排除。

**ConversationRevision**：
process-local单调版本，每次model-visible live mutation递增。ModelCallRequest、logical retry和CompactionPlan使用它验证stale result。它不持久化，不跨restart比较。

**SessionRecorder**：
每个loaded Session一个的有序inline best-effort记录器。`record(entry).await`顺序encode并append当前JSONL line，不使用后台task或queue，也不提供durable commit receipt。

**RecordingHealth**：
Recorder内部状态`Healthy | Degraded { reason, failed_entry_id }`。Create严格stage initial SessionHeader；每次Load都尝试初始化Recorder。Recorder第一次initialize/encode/write失败后Degraded并停止后续记录，replay最多恢复此前有效完整行前缀。Degraded在同一loaded instance内为终态，不retry、不创建segment、不backfill；recording failure不终止Turn、不使Session execution Unavailable。

**SessionRecordingView**：
公开`SessionSnapshot.recording`使用`{ state: healthy | degraded }`。first `Healthy → Degraded`发布一次`session_recording_changed`，同一Snapshot保留至少一条当前脱敏recording diagnostic。raw I/O error、路径和entry内容不公开。

**SessionStorage**：
负责create/open recorded JSONL、tolerant replay、history tree/query，以及从RecordedHistory或LiveSnapshot staging Fork。它不再是loaded conversation truth，也不向async loop签发committed delta。

**StoredSessionEntry**：
SessionRecorder可能写入的一条immutable JSONL record。使用EntryId和parent_id形成recorded history tree。EntryId在live apply时分配，Recorder不能改写。

**Recorded prefix**：
process crash或recording degradation后实际留在JSONL中的完整行前缀。restart只能恢复该prefix，未record live tail永久丢失。

**Tolerant replay**：
顺序读取recorded完整行，skip malformed/duplicate，隔离orphan/invalid relation，排除incomplete Tool exchange并返回bounded diagnostics。不恢复ActiveTurnTask、provider stream、Tool task、waiter、queue或retry timer。

**ForkSourceKind**：
Fork在source linearization point选择的事实来源：loaded Session固定为`LiveSnapshot`，unloaded Session固定为`RecordedHistory`。该值进入child durable fork provenance和`SessionForked`结果。

## Turn与执行

**Turn**：
一次用户意图执行，从live Input UserMessage开始，到Completed/Interrupted/Failed terminal结束。一个Session同时最多一个Running Turn。

**TurnExecutionContext**：
Turn admission时捕获的immutable execution binding，固定AgentRevisionRef、SessionDefinitionRevision、WorkspaceSnapshot、PromptSet、ToolSet、SkillView和TurnModelSnapshot。

**TurnExecutionPhase**：
`Sampling | ExecutingTools | WaitingApproval | WaitingForUserInput | RetryBackoff | Compacting`。只属于live observer state，不记录为Turn lifecycle。

**Async run loop**：
ActiveTurnTask中的普通async Model→Tool→Model流程。first-party MiniCore implementation，不由Rig或其他SDK runner驱动。

**Steer**：
针对exact Running Turn的process-local FIFO输入。只在完整assistant/tool step后、下一次Model前消费一条，并apply为live UserMessage后best-effort record。

**FollowUp**：
等待当前Turn结束后创建新Turn的process-local FIFO输入。新Turn重新capture TurnExecutionContext。

**CancelAccepted**：
确认sticky cancel epoch已发布。它不等待Tool settlement、Turn terminal或Session recording。

**SecurityRevoked**：
WorkspaceAuthority/host发布的process-local emergency signal。阻止新Model/Tool/source operation；Running Tooltruthful settle，Turn结束后重新resolve Workspace。

**ToolStartGate**：
Tool side-effect start与Cancel/SecurityRevoked的owner-local first-wins gate。它不持久化，不依赖SessionRecorder。

**Logical model retry**：
ActiveTurnTask对同一个`Arc<ModelCallRequest>`执行的有限retry。使用control_generation与ConversationRevision验证，backoff可被Cancel/SecurityRevoked打断。

## Prompt、Tool、Skill与Model

**PromptService / PromptSet**：
PromptService拥有definitions/source/cache；每Turn构造immutable PromptSet。PromptSet是`PromptIntent → CanonicalUserMessage`和`LiveConversationView → AssembledModelContext`的唯一seam。

**PromptIntent**：
Text、Template、Skill或Composite结构化用户输入。队列保存intent，不保存提前展开正文。

**CanonicalUserMessage**：
PromptSet规范化产生、可apply到LiveConversation并best-effort record的标准UserMessage。

**PromptContribution**：
Skill/Workspace等module产生的typed User内容。它必须先进入CanonicalUserMessage和LiveConversation，不能作为current-call assembly旁路。

**AssembledModelContext**：
PromptSet产生的唯一provider-neutral模型输入，包含ordered System sections、User context、sanitized messages、ToolSpec、OutputContract和assembly proof。

**ToolService / ToolSet**：
ToolService拥有definition/registry/policy/sandbox/executor；每Turn构造immutable ToolSet。ToolSet只返回ToolExecutionOutcome，不修改LiveSessionState、不写SessionRecorder、不推进async loop。

**ToolCallId**：
由ModelGateway adapter归一化的response-local opaque correlation ID。同一assistant response内唯一；durable/live correlation使用TurnId + ItemId + ToolCallId。

**Complete Tool exchange**：
Assistant的全部expected calls都有first truthful ToolResult后，LiveConversation才把ordered Assistant + ToolResults暴露给下一次Model。recording failure或crash可以留下不完整exchange，cold sanitizer再次执行该规则。

**ToolExecutionControl**：
ActiveTurnTask注入ToolSet的crate-private interface，用于approval、UserQuestion和ToolStartGate。它不暴露Session state。

**SkillService / SkillView**：
SkillService负责discovery、metadata、captured content和cache；Turn-pinned SkillView只使用capture时的immutable source。

**ModelGateway**：
共享深module，通过`resolve_for_turn`固定TurnModelSnapshot，通过`generate_model_turn`执行一个provider attempt。它不拥有Session、conversation、Tool或logical retry。

**ProviderAdapter / RigProviderAdapter**：
ModelGateway private seam，只执行具体provider request/stream/cancellation/response mapping。SDK automatic retry固定为0。

**ModelCallRequest**：
ActiveTurnTask创建的immutable provider-neutral request，包含TurnModelSnapshot、purpose、`Arc<AssembledModelContext>`、source ConversationRevision和effective output limit。

**ModelCallResult / ModelCallError**：
Gateway的一次terminal success或typed failure。ActiveTurnTask验证live basis后apply response；recording outcome不影响provider result真实性。

**StreamingItem**：
Model stream的process-local AgentMessage/Reasoning累积buffer。ProgressEvent使用stable ItemId；provider final成功后apply为live Item并完成inline record attempt。

**CompactionPlan**：
从sanitized LiveConversation和ConversationRevision构建的immutable plan。summary成功后先Replace live conversation，再best-effort record StoredCompaction。

## Turn、Item与Interaction

**Item**：
Turn内稳定可观察对象：UserMessage、AgentMessage、Reasoning或ToolInvocation。final live mutation产生authoritative Item，随后完成inline record attempt。

**Interaction**：
Item执行期间MiniCore发起的ToolApproval或UserQuestion。request先apply live、完成inline record attempt再notify；resolution先apply live、完成inline record attempt再resume。record failure不阻止notify或resume。

**InteractionView**：
公开UI-safe request/resolution view。Presentation Adapter不能创建虚假Pending Interaction或持有Tool waiter。

**StateEvent**：
当前subscription内按序交付的live observer record。final domain event从live mutation派生，可以领先recorded history；restart后不重放。

**ProgressEvent**：
可合并/丢弃的streaming、Tool output或retry update。它不进入LiveConversation或SessionRecorder。

**SessionSnapshot**：
一个loaded Session的live observer baseline，包含execution、current Turn、live Items、Pending Interaction、queues、usage、recording health和diagnostics。它不是durable checkpoint。

**Snapshot-first subscription**：
订阅第一帧返回完整Snapshot，随后交付实时event。断线或背压后重新subscribe；restart后的新Snapshot只基于recorded prefix和new live state。

## 生命周期与Workspace

**Workspace**：
`SessionDefinition.workspace`中的Session-owned definition。没有WorkspaceId或Runtime-global registry。

**WorkspaceSnapshot**：
Turn admission时resolve的immutable Workspace结果。active Turn不读取future Workspace definition。

**SessionReadiness**：
loaded Session是否可admit future Turn。Workspace/Agent revision不可用可导致Unavailable；SessionRecorder Degraded不会。

**SessionExecutionState**：
`Idle | Starting | Running | Finishing`。Running表示ActiveTurnTask存在；Finishing表示停止新逻辑推进并settle。

**Unload**：
停止admission，等待/取消ActiveTurnTask，然后删除loaded handle。Recorder没有后台queue；task结束后不存在待drain record tail。Degraded health与unrecorded live tail随loaded instance销毁。forced process exit可以中断当前append。

**Fork**：
创建新SessionId。loaded source从同一immutable LiveSnapshot解析anchor并复制selected path，因此可以包含unrecorded live tail；unloaded source复制tolerant replay得到的RecordedHistory。Fork不复制task、waiter、queue、Tool process、Recorder object或in-flight append。

## Runtime命令与观察

**RuntimeCommand**：
可信host提交的typed mutation/work request，包括Agent/Session lifecycle、Submit/Steer/FollowUp/Cancel、Resolve Interaction和CommandSurface action。

**CommandId**：
当前process内command correlation和in-flight去重ID。Submit在TurnId创建前也使用CommandId作为Cancel target。不跨restart恢复。

**CommandSurface**：
Runtime内部无状态命令解释module。slash text和GUI catalog selection最终解析为同一typed RuntimeCommand或PromptIntent。

**RuntimeQuery**：
只读typed request。recorded history query与loaded live Snapshot是不同read path。

**Recording degradation**：
Host从`SessionSnapshot.recording.state = degraded`知道当前Session已停止后续记录；`session_recording_changed`提供实时transition，Snapshot中保留当前脱敏diagnostic供重连恢复。

## 已删除术语

以下名称只属于ADR 0126之前的设计，不得用于新实现：

```text
AgentLoop
AgentLoopAction
next_action
accept_committed_tool_results
accept_committed_steer
RunningOperation
OperationResult
SessionWriter
CommittedSessionEntry as execution permit
CommittedConversationView
CommittedToolExchangeDelta
CommittedSteerDelta
ConversationCheckpoint as live proof
Transcript-First
append/apply commit barrier
writer-poisoned Session Unavailable
```

## 当前开放问题

- wire/schema freeze：serde casing、public IDs、Timestamp/Money、StoredTurnStart/StoredCompaction；
- Prompt Q1/Q4：PromptContent representation与contribution stamp字段；
- EntryId generator owner；
- cold recovery closure是否record；
- Rig 0.40.0 provider spike；
- production Tool/Sandbox adapter前关闭O1/R7。

Recorder特有问题见`docs/review/async-loop-best-effort-recording-open-questions.md`。
