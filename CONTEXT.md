# MiniCore Agent Runtime

本上下文描述MiniCore V2当前架构。ADR 0126已经把Turn执行重构为async loop并把Session持久化降级为inline best-effort recording；ADR 0127进一步将JSONL收口为不含Turn lifecycle的conversation recording；ADR 0132冻结Compaction stable-unit/settings/provenance contract；ADR 0133冻结snapshot-recoverable Runtime public payload、安全Interaction和metadata/command completion闭环；ADR 0134/0135冻结bounded wire与host-neutral Workspace input；ADR 0136冻结DurableState/Store V1/root lease（new-entity Create/Fork complete-or-invisible、existing-head update old-or-new），ADR 0137冻结Tokio owner-tracked deterministic foundation。M5.0 design gate已完成；production recovery/root lease/owner-tracked actor与private permanent reservation foundation已实现；Agent Create、ordinary Session Create、unloaded RecordedHistory + Genesis Session Fork tracer及Agent status/definition/metadata、Session metadata CAS、Session definition/Agent revision upgrade CAS、Session lifecycle existing-head action tracer与exact historical Agent/Session definition resolution已通过crate-private owner seams完成publication/recovery或bounded indexed resolution，仍无standalone reservation API/token/receipt。M6.1 crate-private Workspace resolver/immutable Snapshot foundation与loaded Ready+Idle `SessionExecutor` Workspace definition publication owner已实现；PromptSet、actual Workspace source discovery、captured empty SkillView/ToolSet、remaining Fork anchors/LiveSnapshot、Runtime loaded registry/Load/Unload、Session lifecycle Runtime residency integration、public Runtime command、完整native cross-platform matrix、Recorder/replay pending。

权威顺序：`docs/architecture.md`与`docs/modules/` → current/refined ADR → formats + fixtures → development plan → migration + research → archive。

## 核心术语

**MiniCore**：
可嵌入CLI、TUI、GUI可信宿主的原生Agent harness runtime core。负责Session、async Turn执行、Prompt、Tool、Skill、ModelGateway、Runtime协议、观察事件和best-effort recording。

**MiniCoreRuntime**：
下游host唯一顶层门面，通过`dispatch / query / snapshot / subscribe`提供能力。内部拥有PromptService、ToolService、SkillService、ModelGateway和LoadedSessionExecutors，不保存UI selected Session。

**Runtime Interface**：
由RuntimeCommand/CommandResponse、RuntimeQuery/QueryResponse、RuntimeSnapshot/SessionSnapshot和StateEvent/ProgressEvent组成的transport-neutral interface。

**Wire Schema**：
public/storage representation唯一owner。v1固定camelCase fields、snake_case variants、adjacent `type/data`、typed IDs/revisions、Timestamp/Duration/Money/path/cursor、ProtocolLimits、canonical BoundedJson和bounded scanner；不拥有domain business semantics。

**Wire V1 Fixtures**：
`docs/fixtures/wire-v1/`中的public target manifest、byte-exact JSON/JSONL、corruption expectations、boundary recipes和structural verifier。首个Rust codec/storage crate必须消费这些assets。

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
loaded Session的current-process truth，保存live conversation、Turn、Item、Interaction、usage和read model。它保留完整`Vec<Arc<StoredSessionEntry>>` selected path、由末项导出selected head；EntryId-only不能materialize unrecorded live tail供future LiveSnapshot/Fork。all fallible preparation结束后才allocate；allocation后先construct exact entry Arc、再bind prepared new-origin stable unit、commit state、append returned `AppliedConversationFact`的**same Arc**到path并install preflighted revision，绝不在entry construction前apply state。它通过private typed methods修改；`Interaction` fields/raw state只由其request/resolution transition methods构造和改变，sibling只读safe projection；任何lock guard不得跨await。

**LiveConversation**：
模型协议安全的current-process conversation reducer。它拥有expected ToolCalls、first terminal result、complete exchange、Compaction Replace和ConversationRevision；`ModelMessage`构造/provider projection仍只调用Prompt-owned constructors。

**LiveConversationView**：
PromptSet可消费的sanitized只读view。private fields，只有crate-private revision/messages getter；LiveSessionState在`capture_conversation_views()`中构造。只包含provider-valid messages；incomplete、orphan或abandoned-first Tool exchange被排除。M4没有generic live/replay trait或public DTO。

**LiveCompactionSourceView**：
Live reducer额外提供的crate-private immutable Compaction projection。`LiveCompactionUnit`与source view是private-field Arc-backed `Clone` handles，clone保持origin/kind/order/message semantic identity，不重建unit。Compaction语义拥有`PreparedLiveCompactionUnit::for_live_reducer(kind, messages) -> Result<_, CompactionSourceError>`和infallible `bind_origin(self, EntryId) -> LiveCompactionUnit`，以及source factory/read getters和唯一deep `has_same_stable_identity(&self, other: &Self) -> bool` method；preparation在new ID allocation前完成all message/kind validation，source factory仍返回own redacted `CompactionSourceError { EmptyUnitMessages | DuplicateUnitOrigin | MisplacedRollingSummary }`。它绝不返回或依赖`LiveConversationError`。LiveSessionState仍是canonical producer，并在factory caller boundary映射该error到own typed live error。该method精确比较Session/revision、unit count与ordered`(first_entry_id, kind)` sequence，绝不比较message value；source不存储或暴露identity DTO。Tool exchange不可拆，rolling summary origin是对应StoredCompaction outer EntryId；retained suffix只clone fresh current source units。它不携带token estimate或settings。

**CapturedConversationViews**：
`LiveSessionState::capture_conversation_views()`一次短capture返回`Result`的crate-private aggregate：同一revision的LiveConversationView、`Arc<LiveCompactionSourceView>`、从full state path末项导出的selected head、`Arc<[ItemRelation]>`和RequestId/TurnId/ItemId + safe request view的`Arc<[PendingInteractionFact]>`。M4 read scope只暴露head；state保留full applied facts给future LiveSnapshot/Fork。private fields/getters only；不是M8 public Snapshot，也不是Fork LiveSnapshot。

**CompactionSettingsSnapshot**：
Turn admission从Runtime-global validated CompactionSettings捕获的immutable policy。MVP无hot reload或per-Session override；默认pressure reserve 4096、summary output 512–2048、minimum reclaim 2048、每Turn最多4次Compaction、summary safety reserve 512。

**ConversationRevision**：
process-local单调live-conversation operation basis，不是当前可见消息的hash/version。Input/Steer与每个accepted Assistant（含hidden ToolCalls）各`+1`；complete exchange在所有expected calls first truthful Completed时再`+1`；partial/abandoned/non-visible settlement、Interaction、progress/usage/recording与failed/idempotent apply均`+0`；Compaction Replace `+1`。checked overflow先于EntryId allocation/state mutation失败。ModelCallRequest、logical retry和Compaction source/plan使用它验证stale result；它不持久化，不跨restart比较。

**EntryIdGenerator**：
`LiveSessionState`私有持有的Session-scoped identity generator。`allocate()`是typed fallible operation：16 CSPRNG-byte candidate最多32次、unique candidate在return前reserve；entropy或collision exhaustion是owner-local redacted error，不panic且不改变state/head/revision。domain validation和revision overflow preflight后、live apply前分配并绑定parent_id；replay/Fork全部reserved copied IDs seed collision guard。Degraded不影响分配，Recorder不能创建或改写ID，也不得从revision/ordinal/time派生ID。

**SessionRecorder**：
每个loaded Session一个的有序inline best-effort记录器。`record(entry).await`顺序encode并append稳定conversation fact，不使用background queue或durable commit receipt。其filesystem blocking job由owner追踪/join；started append不因Cancel/Unload/drop而detach，terminal/unload等待settlement。TurnStatus与terminal reason不进入Recorder。

**RecordingHealth**：
Recorder内部状态`Healthy | Degraded { reason, failed_entry_id }`。Create严格stage initial SessionHeader；每次Load都尝试初始化Recorder。Recorder第一次initialize/encode/write失败后Degraded并停止后续记录，replay最多恢复此前有效完整行前缀。Degraded在同一loaded instance内为终态，不retry、不创建segment、不backfill；recording failure不终止Turn、不使Session execution Unavailable。

**SessionRecordingView**：
公开`SessionSnapshot.recording`使用`{ state: healthy | degraded }`。first `Healthy → Degraded`发布一次`session_recording_changed`，同一Snapshot保留至少一条当前脱敏recording diagnostic。raw I/O error、路径和entry内容不公开。

**DurableState**：
private deep module，拥有local Store V1、permanent ID reservation、root lease、single actor、generation/CAS/marker publication/readback、catalog recovery/cleanup、poison和filesystem fault seam。它不暴露staging/path/generation/marker；`CommandId`不进入它。

**ConversationStorage**：
拥有SessionHeader/JSONL、tolerant replay、history tree/query和Fork semantic seed。它通过DurableState-issued opaque published target、RecordedHistory lease和root-lease-derived writable proof工作，不拥有entity path/publication，也不向async loop签发committed delta。

**StoredSessionEntry**：
SessionRecorder可能写入的一条immutable Format V1 conversation entry。exact wire fields依次为`entryId`、`parentId`、`sessionId`、`turnId`、`timestamp`和`body`；body是User/Assistant/Tool/InteractionRequested/InteractionResolved/Compaction六种snake_case flat variants。EntryId由live owner在apply前分配，Recorder不能创建或改写。

**Recorded prefix**：
process crash或recording degradation后实际留在JSONL中的完整行前缀。restart只能恢复该prefix，未record live tail永久丢失。

**Tolerant replay**：
顺序bounded读取recorded完整行，strict Header；session match先于EntryId collision reservation，随后skip duplicate并隔离orphan/invalid relation。first valid root建立canonical component，component内physical-last accepted leaf决定selected path；排除incomplete Tool exchange并返回typed bounded diagnostics。不恢复ActiveTurnTask、provider stream、Tool task、waiter、queue、retry timer或旧TurnStatus；Load后的current Turn为空。

**ForkSourceKind**：
Fork在source linearization point选择的事实来源：loaded Session固定为`LiveSnapshot`，unloaded Session固定为`RecordedHistory`。该值进入child durable fork provenance和`SessionForked`结果。

## Turn与执行

**Turn**：
一次current-process用户意图执行，从live Input UserMessage开始，到Completed/Interrupted/Failed terminal结束。一个Session同时最多一个Running Turn。JSONL只用TurnId分组conversation facts，不保存Turn lifecycle。

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
确认sticky cancel epoch已发布。Starting阶段保持`Submit CommandId` target：Input live apply前取消candidate，apply后绑定同一Turn并阻止ActiveTurnTask spawn；response publication后使用TurnId。它不等待Tool settlement、Turn terminal或Session recording。

**SecurityRevoked**：
WorkspaceAuthority/host发布的process-local emergency signal。阻止新Model/Tool/source operation；Running Tooltruthful settle，Turn结束后重新resolve Workspace。

**ToolStartGate**：
Tool side-effect start与Cancel/SecurityRevoked的owner-local first-wins gate。它不持久化，不依赖SessionRecorder。

**Logical model retry**：
ActiveTurnTask对同一个`Arc<ModelCallRequest>`执行的有限retry。使用control_generation与ConversationRevision验证，backoff可被Cancel/SecurityRevoked打断。

## Prompt、Tool、Skill与Model

**PromptService / PromptSet**：
PromptService拥有definitions/materialized content/source/cache；每Turn构造immutable PromptSet。PromptSet是`PromptIntent → CanonicalUserMessage`和`LiveConversationView → AssembledModelContext`的唯一seam。

**PromptContent**：
Prompt candidate build期间已经读取、解析和规范化的immutable text value。多个definition/Turn可以通过进程内强`Arc`共享正文；path、URL、source ID、hash或cache key不承担正文resolver或durable identity。

**PromptIntent**：
用户body与ordered SkillIntent selections组成的结构化输入。MVP body只有Empty或non-empty Text；不定义Template、Skill或Composite顶层variant。队列保存intent，不提前展开Skill正文。

**SkillIntent**：
显式请求本次用户消息使用某个Skill的稳定选择，只保存SkillId；name、path与source authorization不属于intent。

**CanonicalUserMessage**：
PromptSet规范化产生、可apply到LiveConversation并best-effort record的标准UserMessage。

**PromptContribution**：
Skill/Workspace等module产生的typed User内容。exact source authorization在composition前验证；每个contribution形成独立顶层content part并进入CanonicalUserMessage和LiveConversation，不能作为current-call assembly旁路。

**PromptContributionStamp**：
通过`content_part_index`关联一个顶层content part的安全解释元数据。只保存SkillId或WorkspaceRootKey加relative location；不保存字符offset、绝对路径、authorization或正文引用。

**ModelMessage**：
Prompt拥有的crate-private opaque唯一provider-neutral transcript；Prompt alone construct/destructure private kinds。`ModelMessage`与`ModelAssistantContent`是immutable Arc-backed `Clone` values，clone保持semantic identity/order/provenance，是将同一message投影到stable unit和flattened LiveConversationView的唯一方式。read-ref enums和`as_ref()`都不是external API；ProviderAdapter、Compaction estimator/reduction及Prompt assembly/tests等authorized consumers只能inspect `ModelMessageRef`/`ModelAssistantContentRef`：`ModelMessageRef::User { content: &[MessageContent] }`不含stamp，且stamp通过refs不可能访问；Assistant是ordered opaque content；Tool是`ToolCallId + ToolResultContent`。`ModelAssistantContentRef`只读Reasoning、Text或`{ tool_call_id, name, arguments }`。完整ReasoningContent含portable `provider_item_id`这一明确允许的fixture/storage exception；response ID、stream/final index/order bookkeeping、metadata、usage等provider-attempt facts禁止进入。public `PromptValueError`保持不变；transcript constructors返回private redacted `ModelMessageError { EmptyText | UnsafeText | TextTooLong | EmptyAssistantContent | DuplicateToolCallId }`。`rolling_summary()`只可达text reasons（含任意CR或CRLF、无normalization），accepted summary恰为一条unstamped verbatim User/Text，无label/envelope/stamp；assistant constructor独立覆盖empty/duplicate reasons。`unstamped_user_text()`保持独立静态context规则。Storage/Wire/Compaction不得定义shadow transcript。

**AssembledModelContext**：
PromptSet产生的唯一provider-neutral模型输入，包含ordered System sections、User context、sanitized messages、ToolSpec、OutputContract和assembly proof；没有flat contribution_stamps。stamp只留在各User ModelMessage，既非provider payload/cache-control input，也非source locator/authorization。

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
M10从exact `LiveCompactionSourceView`、Turn-captured settings、Prompt assembly bases和TurnModelSnapshot basis构建的immutable plan。只保存source + stable-unit cut，summary prefix、retained suffix和`first_kept_entry_id`均由cut派生；M4 `CompactionReplacement`只有crate-test factory和consuming exact `into_parts()`，M10才在`ValidatedCompactionSummary`存在时增加production construction。reducer consume后可clone the prebuilt immutable rolling summary into leading unit/flattened view，retained units只clone from fresh current source，均不重建borrowed message或caller suffix。all fallible preparation/checked-next preflight先于allocation；之后construct exact entry Arc → bind prepared rolling-summary origin → commit Replace → append same Arc → install revision → best-effort record. M4只关闭无await/no-I/O reducer subset（CompactionReplacement + source + cut + orchestration-supplied TurnId/Timestamp）；M5拥有bad recorded marker ignore/diagnose；automatic M10 model-call provenance始终为Some。

## Turn、Item与Interaction

**Item**：
Turn内稳定可观察对象：UserMessage、AgentMessage、Reasoning或ToolInvocation。final live mutation产生authoritative Item，随后完成inline record attempt。

**Interaction**：
Item执行期间MiniCore发起的ToolApproval或UserQuestion。ordinary message apply只收valid-by-construction Stored User/Assistant/Tool body，连同orchestration-supplied `TurnId + Timestamp`，不定义message candidate；Interaction是唯一private-candidate exception：request为RequestId + ItemId + owner `InteractionRequest`，连同`TurnId + Timestamp`，resolution为RequestId + optional host key + opaque owner `ResolvedInteraction`，只另收`Timestamp`。`Interaction` fields/raw state只属于LiveSessionState；its transition methods alone construct/resolve it，siblings只读safe projection。`host(...) -> Result<_, InteractionCandidateError>`只接受ToolApproval/UserAnswer或Cancelled(HostCancelled)并seal Some key；`owner_cancellation(...) -> Result<_, InteractionCandidateError>`只接受Cancelled non-Host并seal None，wrong origin在apply/EntryId allocation前拒绝。reducer从exact stored pending request导出resolution的TurnId/Item/family、safe stored body并保留private resolution。reducer绑定SessionId/EntryId/parent并验证supplied TurnId的current/start semantics；Timestamp是Session/Turn orchestration提供的typed fact，绝不读ambient clock，Input start之前也不宣称TurnId/timestamp可从state导出。每Item最多一个Pending Interaction；terminal resolution后允许顺序later interaction。request/resolution先apply live、完成inline record attempt再notify/resume，但它们model-invisible且ConversationRevision `+0`。same-key/same-payload resolution是no-ID/no-entry/no-record/no-event的idempotent outcome。record failure不阻止notify或resume。

**InteractionView**：
公开UI-safe request/resolution view。Presentation Adapter不能创建虚假Pending Interaction或持有Tool waiter。

**StateEvent**：
当前subscription内按序交付的live observer record。final domain event从live mutation派生，可以领先recorded history；Turn terminal StateEvent不进入JSONL，restart后不重放。

**ProgressEvent**：
可合并/丢弃的streaming、Tool output或retry update。它不进入LiveConversation或SessionRecorder。

**SessionSnapshot**：
一个loaded Session的live observer baseline，包含execution、current Turn、live Items、Pending Interaction、queues、usage、recording health和diagnostics。它不是durable checkpoint。

**Snapshot-first subscription**：
订阅第一帧返回完整Snapshot，随后交付实时event。断线或背压后重新subscribe；restart后的新Snapshot只基于recorded conversation prefix和new live state，`current_turn`为空。

## 生命周期与Workspace

**Workspace**：
`SessionDefinition.workspace`中的Session-owned definition。没有WorkspaceId或Runtime-global registry。

**WorkspaceRootInput**：
public Create/Update命令中的host-neutral Workspace root intent，具体字段为`path: CanonicalFileUri`；它不是durable native path。typed command进入Runtime后由Workspace按current host checked-lower为`WorkspaceRootSpec { path: PathBuf }`；unsupported family是accepted command的InvalidArgument，不是wire decode failure。

**WorkspaceRootSpec**：
durable `Workspace`中的current-host native root definition。只能由Workspace checked lowering或trusted native constructor形成，不越过public input seam。

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
当前process内command correlation和in-flight去重ID。Submit在TurnId创建前也使用CommandId作为Cancel target。不跨restart恢复。SessionSnapshot完整列出当前可取消Submit/Steer/FollowUp CommandId，不公开queued prompt正文。

**InteractionResolutionKey**：
Presentation Adapter为一次logical Resolve生成的不可预测random 128-bit key。exact request内same key/same canonical payload幂等；same key/different payload冲突；不同key不能覆盖terminal resolution。它不是approval capability。

**Metadata revision**：
AgentMetadataRevision与SessionMetadataRevision分别为metadata CAS token；与AgentRevision/SessionDefinitionRevision正交。Create/read/outcome/event闭合下一次UpdateMetadata所需token。

**CommandSurface**：
Runtime内部无状态命令解释module。slash text和GUI catalog selection最终解析为同一typed RuntimeCommand或PromptIntent。

**RuntimeQuery**：
只读typed request。recorded history query与loaded live Snapshot是不同read path。

**Recording degradation**：
Host从`SessionSnapshot.recording.state = degraded`知道当前Session已停止后续记录；`session_recording_changed`提供实时transition，Snapshot中保留当前脱敏diagnostic供重连恢复。

## 已删除术语

以下名称或语义已经被ADR 0126/0127删除，不得用于新实现：

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
StoredTurnStart
StoredTurnTerminal
HistoricalFork terminal closure
cold recovery Turn terminalization
PromptIntent::Skill / PromptIntent::Composite / PromptBodyIntent::Template
```

## 当前开放问题

- 第四轮评审：全部V4-P0、V4-P1-1、V4-P1-2与V4-P1-4已关闭；V4-P1-3仍开放；
- M0、M1、M2 minimal Snapshot/Event、M3.1、M3.2和M4已完成；M1 Wire foundation/owner semantic spine已完成并通过Fast、MSRV与heavy gates；M2仍按slice增量推进，已完成Protocol V1 bootstrap router、incremental public manifest conformance gate、initial typed Wire roots、M7 Create/Load/Unload/Submit/Cancel command codec、TurnStarted/CommandOutput/typed rejection completion，以及minimal loaded-ready-idle SessionSnapshot与Runtime/Turn terminal StateEvent codec；M3.1已完成strict Header、六种flat body的exact Conversation Header/Entry per-line codec、bounded duplicate-aware preflight与raw ToolCall `arguments` cap、owner/writer invariants和全部conversation golden的byte-exact round-trip；M3.2仅完成bounded streaming scanner（known size/stat unavailable 1 GiB cap、LF/CRLF、strict Header、line/count limits、recovery，以及要求opaque `ExclusiveWritableConversationLease`才返回final partial-tail truncation action/offset）；M5.0 design gate、recovery/root lease/owner-tracked actor、permanent reservation foundation、Agent Create、ordinary Session Create、unloaded RecordedHistory + Genesis Session Fork exact G1 publication tracer、Agent status/definition/metadata、Session metadata CAS、Session definition/Agent revision upgrade CAS、Session lifecycle existing-head action tracer与exact historical Agent/Session definition resolution已完成；M6.1 resolver/Snapshot foundation及crate-private loaded Ready+Idle Workspace definition publication owner已完成，覆盖Idle/Starting exclusion、caller drop、pre/post-commit barrier、Snapshot atomic install、post-commit poison、shared task cancellation-safe join与close drain；Agent/Session Create/Fork Unix process-abort前后PUBLISHED tracer已在macOS本地验证，自动化Linux/macOS/Windows native matrix、remaining Fork anchors/LiveSnapshot、Runtime loaded registry/Load/Unload、Session lifecycle Runtime residency integration、public Runtime command、Recorder/replay pending；M4已完成Prompt-owned opaque `ModelMessage`、exact `ConversationRevision`/`EntryIdGenerator`、`LiveSessionState`的User/Assistant/Tool/Interaction reducer、complete Tool exchange、coherent capture与Compaction stable units/source/replacement subset，关闭INV-003和INV-005 reducer subset（source/cut/marker/no-I/O）。M5拥有tolerant recorded-marker replay，M10才完成full Compaction；M4 read scope仍为crate-private snapshot，public Item/Interaction DTO随M8激活；
- V4-P1-3：production provider scope与Rig 0.40.0 reality/mock-server spike；
- production Tool/Sandbox adapter前关闭O1/R7。

Recorder特有问题见`docs/review/async-loop-best-effort-recording-open-questions.md`。
