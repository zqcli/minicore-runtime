# MiniCore 架构（V2 当前权威）

本文档是MiniCore原生Agent harness runtime core的架构总入口。详细设计位于[`docs/modules/`](modules/README.md)。

## 版本状态

| 版本 | 状态 |
| --- | --- |
| V1 | 已归档，只保存在[`docs/archive/v1/`](archive/v1/README.md)和Git history中。 |
| V2 | 当前权威架构。ADR 0126–0135冻结async execution、conversation/wire与public payload基础；ADR 0136冻结DurableState/Store V1/root lease（new-entity Create/Fork complete-or-invisible、existing-head update old-or-new），ADR 0137冻结Tokio owner-tracked foundation。M5.0 recovery、root lease、owner-tracked actor、permanent reservation inventory、crate-private Agent Create与ordinary Session Create exact G1 COMMITTED/PUBLISHED publication、unloaded RecordedHistory + Genesis Session Fork tracer，以及Agent status/definition/metadata、Session metadata CAS、Session definition/Agent revision upgrade CAS与Session lifecycle existing-head action tracers已实现；public Runtime command接入、remaining Fork anchors/LiveSnapshot、exact historical Agent/Session definition resolution、loaded Workspace Idle/Snapshot publication、Session lifecycle Runtime residency integration及完整cross-platform native matrix仍待实现；后续见[开发计划](development-plan.md)。 |

权威顺序：本文与`docs/modules/` → current/refined ADR → formats + fixtures → development plan → migration + research → archive。

## 设计定位

MiniCore采用Codex式执行结构：每个loaded Session有一个`SessionExecutor` control actor和最多一个`ActiveTurnTask`。ActiveTurnTask使用普通async loop顺序编排Model、Tool、Interaction、logical retry和Compaction；不再实现同步sans-I/O `AgentLoop`、`next_action()`或`RunningOperation` effect协议。

`DurableState`是Agent/Session entity physical truth的private deep module：它以root lease、permanent reservations、immutable Store V1 generations、COMMITTED/PUBLISHED readback和single actor管理catalog；caller永不看到staging/path/generation/marker。新entity Create/Fork publication是complete-or-invisible；existing-head update则在reopen时是完整old generation或完整new generation。`CommandId`不持久化，Create/Fork response loss可能留下host未知但catalog-visible的generated ID，host需重新page/query且blind retry可能duplicate。

Session的当前进程事实由`LiveSessionState`拥有。`SessionRecorder`只做ordered best-effort inline JSONL append；成功不表示flush或fsync，失败不回滚live state或重放外部操作。`ConversationStorage` owns the recorded tree, replay, and Fork semantic seed; `DurableState` owns the physical target, Fork sink, and publication; lifecycle/Runtime orchestrates them. TurnStatus与terminal StateEvent只属于loaded execution。process crash后只恢复实际留下的conversation完整行前缀。

Rig只实现`ModelGateway` private `ProviderAdapter`的单次provider attempt。Model resolution、request validation、credential、response validation和provider-neutral terminal result仍由ModelGateway拥有；logical retry由ActiveTurnTask拥有。

下游CLI、TUI和GUI只通过`MiniCoreRuntime`的command、query、snapshot和event interface接入。

## 领域模型

```text
MiniCoreRuntime
└─ Agent*
   └─ Session*
      └─ Turn*
         └─ Item*
            └─ Interaction*
```

核心关系：

- 一个Agent可被多个Session引用；一个Session固定归属一个Agent；
- Workspace属于`SessionDefinition`，active Turn捕获immutable `WorkspaceSnapshot`；
- Prompt、Tool和Skill是独立module；
- Turn/Item/Interaction是领域对象，Model request、provider stream、Tool future和ActiveTurnTask是process-local执行对象；
- Session log用于resume、history和诊断，不证明当前进程全部live事实已经durable；
- restart不恢复旧ActiveTurnTask、provider stream、Tool task、Interaction waiter、retry timer或queue。

## Runtime-Owned共享模块

```rust
pub struct MiniCoreRuntime {
    prompt_service: Arc<PromptService>,
    tool_service: Arc<ToolService>,
    skill_service: Arc<SkillService>,
    model_gateway: Arc<ModelGateway>,
    shared_resources: RwLock<SharedResourceRoots>,
}
```

```text
captured SharedResourceRoots + Session/Agent/Workspace facts
├─ ModelGateway::resolve_for_turn → Arc<TurnModelSnapshot>
├─ SkillService::for_turn         → Arc<SkillView>
├─ ToolService::for_turn          → Arc<ToolSet>
└─ PromptService::for_turn        → Arc<PromptSet>
```

active Turn始终使用admission时捕获的immutable对象。Prompt source在candidate build期间完全materialize为强`Arc`持有的正文；path/URL/source ID只用于discovery或provenance，PromptSet不执行正文I/O或resolver lookup。显式reload只影响future Turn。

## Loaded Session结构

```text
MiniCoreRuntime
└─ LoadedSessionExecutors
   └─ SessionExecutor
      ├─ SessionIngress
      ├─ LiveSessionState
      ├─ SessionRecorder
      ├─ SessionSnapshot publisher
      └─ optional ActiveTurnTask
```

`SessionExecutor`拥有Session级control、lifecycle、FollowUp、active-task handle和公开snapshot。`ActiveTurnTask`拥有当前Turn的async control flow、phase、Model/Tool future、retry timer和compaction orchestration。

`LiveSessionState`通过private typed methods更新。若使用锁，guard不得跨任何I/O或await。SessionExecutor与ActiveTurnTask不得分别维护可独立修改的conversation副本。

## Session Recording

recordable conversation mutation顺序：

```text
complete all relation/value validation, Prompt projection and candidate preparation
→ preflight ConversationRevision.checked_next() when delta is +1
→ LiveSessionState.EntryIdGenerator.allocate()? and bind parent_id
→ infallibly construct exact Arc<StoredSessionEntry>
→ infallibly bind any prepared new-origin stable unit, commit prepared LiveSessionState / LiveConversation delta
→ append the same Arc to the full selected path and install the preflighted revision
→ await SessionRecorder.record(the same Arc)
→ publish final StateEvent / resume waiter / continue loop
```

所有normal-result fallible validation/projection/preparation（包括`PreparedLiveCompactionUnit`）都在allocation前完成；revision overflow与EntryId allocation failure都在state/head mutation前返回typed failure。allocation后先构造exact entry Arc，才bind prepared unit、commit state、append same Arc和install revision；这些步骤不会返回错误（ordinary allocation panic在Result contract外），所以returned error绝不消耗ID。EntryId使用CSPRNG、success前reserve和32次bounded collision retry，不能panic或从revision/ordinal/time派生。

EntryId由`LiveSessionState`私有Session-scoped generator在apply前分配；Recorder不得创建或改写identity。`record().await`顺序encode并执行当前JSONL line的`write_all`，不使用后台queue。成功不表示flush、fsync或power-loss durability。第一次encode/write失败后Recorder进入`Degraded`并停止该loaded Session的后续记录；Turn继续运行。Degraded在同一load内为终态，不retry、不创建segment、不backfill。Interrupted/Failed terminal没有record attempt，Completed通过Final Assistant conversation entry留下内容事实。

Cold replay：

- 顺序读取完整JSONL行；
- skip malformed/duplicate并隔离orphan或invalid relation；
- 重建recorded history和sanitized model conversation；
- incomplete Tool exchange不进入模型conversation；
- 不从recorded TurnId重建旧TurnStatus，不追加restart closure；
- writable Load只截断final unterminated partial tail，再从replayed recorded head初始化新Recorder；
- Load完成后`current_turn = None`并进入Idle或Unavailable；
- Unload/Load只恢复recorded conversation prefix，未record的live tail永久丢失。

## Turn执行

```text
Submit admission
→ capture TurnExecutionContext
→ apply live Input + await inline record attempt
→ spawn ActiveTurnTask
→ async run_turn loop
   ├─ consume safe-point Steer
   ├─ PromptSet.assemble(LiveConversationView)
   ├─ await ModelGateway
   ├─ await ToolSet / Interaction
   ├─ logical retry or Compaction
   └─ final arbitration
→ return TurnTaskOutcome
→ SessionExecutor settles lifecycle and FollowUp
```

Turn creation在Input live apply时线性化，Agent lifecycle/admission permit在Recorder await前释放；`Starting`持续到Input record attempt和`TurnStarted` publication完成。该窗口内`Cancel(Submit(command_id))`继续有效，Input已apply时绑定同一Turn并阻止task spawn。

同Session只有一个ActiveTurnTask。多个Session可以同时调用共享ModelGateway；Gateway没有本地模型调用permit。

## Conversation与Tool Exchange

模型输入只来自sanitized `LiveConversationView`：

```text
Assistant(tool calls A/B/C) applied live
→ Tool A/B/C may run
→ results applied in completion order
→ all expected calls have first truthful ToolResult
→ reducer exposes ordered Assistant + Result A/B/C exchange
→ next Model allowed
```

complete exchange门禁保留，但owner是live conversation reducer。SessionRecorder或cold projector不再向执行loop签发`CommittedToolExchangeDelta`。accepted Assistant（包括含ToolCalls、仍被gate隐藏者）使`ConversationRevision +1`；所有expected calls的first truthful `Completed` result才使complete exchange promotion再`+1`，partial/abandoned/non-visible settlement为`+0`。

每个assistant或ToolResult live mutation在启动下一protocol step前完成inline record attempt。crash或Degraded仍可能只留下assistant ToolCall或部分结果；replay sanitizer排除整个不完整exchange。Tool不会因记录缺失自动重跑。

## Interaction

```text
apply Pending Interaction live
→ await inline record request attempt
→ publish InteractionView
→ await oneshot resolution
→ validate and apply resolution live
→ await inline record resolution attempt
→ resume waiter / authorize Tool
```

recording failure不阻止Interaction。request/resolution可能在crash后缺失；restart不恢复waiter。

`Interaction` fields/raw state只属于`LiveSessionState`；only its request/resolution transition methods construct、resolve或match该state，sibling只能读safe pending/event/storage projection。`InteractionResolutionCandidate::host(...) -> Result<_, InteractionCandidateError>`只接受ToolApproval、UserAnswer或`Cancelled(HostCancelled)`并seal Some key；`owner_cancellation(...) -> Result<_, InteractionCandidateError>`只接受non-Host `Cancelled`并seal None。wrong origin在reducer apply与EntryId allocation前被redacted candidate error拒绝。

同一Interaction的same key + same canonical payload resolution是live reducer idempotent outcome：不分配EntryId、不构造entry、不record或发第二event；same key + different payload保持typed conflict。

## Cancel与Security

- Cancel发布sticky epoch后立即返回`CancelAccepted`；
- ActiveTurnTask停止新Model、Tool和Compaction；
- Running Tool执行best-effort cancel并truthful settle；
- FollowUp等待旧task terminal后再启动；
- Tool start继续通过`ToolStartGate`与EmergencyControl first-wins；
- Workspace update仍只在Idle；SecurityRevoked后重新resolve失败仍可进入Unavailable。

## Logical Retry

Logical retry是ActiveTurnTask局部流程：

```text
retryable terminal ModelCallError
→ verify Turn/control_generation/conversation_revision
→ cancellation-aware sleep
→ reuse same Arc<ModelCallRequest>
→ invoke ModelGateway again
```

不再存在`RunningOperation::WaitForModelRetry`或基于durable `ConversationCheckpoint.entry_id`的live校验。

## Compaction

ActiveTurnTask从`LiveSessionState::capture_conversation_views()`在同一revision取得ordinary `LiveConversationView`、额外的`LiveCompactionSourceView`、由state完整immutable selected path末项派生的selected head和private Item/Pending Interaction facts。M4 aggregate只读该head；state保留full `Arc<StoredSessionEntry>` path给future LiveSnapshot/Fork。后者按EntryId-bearing provider-valid stable units表达User、Assistant、complete Tool exchange和leading rolling summary；Tool exchange不可拆，rolling summary origin是对应StoredCompaction outer EntryId。

Compaction使用Turn-captured Runtime settings、同一PromptSet的AgentRun/Summary assembly bases和同一TurnModelSnapshot estimator/limits，从`source + cut index`派生summary prefix、retained suffix和single `first_kept_entry_id`，不按message equality反查。`ModelMessage`/`ModelAssistantContent`和`LiveCompactionUnit`/source view都是immutable `Clone` values/handles；Compaction estimator/reduction只读transcript refs。Compaction owns fallible `PreparedLiveCompactionUnit` construction before an origin exists and source factories return its redacted structural error（empty unit、duplicate origin、misplaced rolling summary）；only `LiveSessionState` maps it to an owner-local live error, so Compaction never depends on `LiveConversationError`. **M4只关闭reducer subset**：apply接收orchestration-supplied `TurnId + Timestamp`，创建fresh current source，以`source.has_same_stable_identity(&fresh_current_source)`验证exact SessionId/revision/unit-count/ordered `(first_entry_id, kind)` basis，连同nonzero cut和opaque `CompactionReplacement` proof验证derived marker。M4 proof only has `#[cfg(test)] for_m4_test(...) -> Result<_, CompactionReplacementError>` and consuming exact `into_parts()`; it has no production `ValidatedCompactionSummary` constructor. Reducer consumes its exact values, may clone the prebuilt summary into leading unit and flattened LiveConversationView, and prepares the summary unit before all validation/projection/candidate preparation and `checked_next()`. Only then it allocates a new rolling-summary origin, constructs the exact entry Arc, binds the prepared unit, clones only the retained unit handles from the fresh current suffix, commits state, appends that same Arc and installs the preflighted revision; it never reconstructs borrowed messages or a caller suffix. M10 will add production construction from `ValidatedCompactionSummary` when those types exist. M5对recorded bad marker只ignore/diagnose。summary调用、full plan/request/control validation、model validation/retry、recorder ordering与publication仍是M10；marker未写入时restart恢复未压缩旧conversation是该best-effort M10路径的降级。

## 状态与观察

- `SessionExecutionState = Idle | Starting | Running | Finishing`；
- `TurnExecutionPhase = Sampling | ExecutingTools | WaitingApproval | WaitingForUserInput | RetryBackoff | Compacting`；
- StateEvent和Snapshot描述live state；recording Degraded或process crash时，它们可能领先可恢复的recorded prefix；Healthy状态没有后台queue lag；
- ProgressEvent仍可合并或丢弃；
- `SessionSnapshot.recording.state = healthy | degraded`；每个loaded Session都尝试记录，first `Healthy → Degraded`先由当前domain event发布Degraded Snapshot，再补发一次`session_recording_changed`；Snapshot保留当前脱敏recording diagnostic；
- StateEvent不是durable acknowledgement。

## 跨模块不变量索引

只有跨至少三个module且影响correctness/security的规则进入本表。

| ID | 不变量摘要 | Canonical Owner |
| --- | --- | --- |
| INV-001 | live owner为recordable conversation fact在apply前分配稳定EntryId并绑定parent，apply后完成inline record attempt再publish/推进；TurnStatus只apply/publish live，Recorder不得创建identity或terminal | [Conversation Recording · Live Mutation](modules/conversation-storage.md#live-mutation-and-recording) |
| INV-002 | cold replay只恢复recorded完整行前缀，局部skip/isolate并返回diagnostics，不恢复process-local对象 | [Conversation Recording · Tolerant Replay](modules/conversation-storage.md#tolerant-replay) |
| INV-003 | 含ToolCall的assistant只有在全部matching truthful results形成provider-valid complete exchange后才model-visible | [Turn / Item / Interaction · Complete Tool Exchange](modules/turn-item-interaction.md#complete-tool-exchange) |
| INV-004 | loaded Fork从同一LiveSnapshot解析anchor并 semantic-stream-re-encodes selected path；unloaded Fork使用RecordedHistory lease；child Header is new and only entry SessionId rebinds while historical IDs/body/order persist; source kind进入durable provenance与command outcome | [Conversation Recording · Fork](modules/conversation-storage.md#fork) |
| INV-005 | Compaction source由live reducer发布EntryId-bearing stable units；`has_same_stable_identity()`比较Session/revision/unit count/ordered unit identity，cut派生marker；M4以opaque CompactionReplacement关闭source/cut/marker/no-I/O reducer subset，M10才关闭exact control/plan/request、summary validation、recording与publication | [Compaction · M10 Live Replace与Recording](modules/compaction.md#m10-live-replace与recording) |
| INV-006 | DurableState root lease下，新entity Create/Fork publication是complete-or-invisible，caller永不取得staging/path/generation/marker；existing-head update在reopen时只能是完整old或完整new generation。`PUBLISHED`是新entity唯一catalog root，marker ambiguity以exact readback决定Published或poison/Runtime close | [DurableState · CAS, generations and publication](modules/durable-state.md#cas-generations-and-publication) |
| INV-101 | 每个loaded Session只有一个control actor和最多一个ActiveTurnTask；同Session不得并行运行两个Turn task | [Session Execution · Ownership](modules/session-execution.md#ownership) |
| INV-102 | Steer只在完整assistant/tool step后、下一次Model前FIFO消费 | [Session Execution · Steer](modules/session-execution.md#steer) |
| INV-103 | SessionSnapshot完整列出当前process所有public cancelable Submit admission、Steer和FollowUp CommandId；lane内FIFO，不公开queued intent正文，不从event/count重建 | [Runtime Interface · SessionSnapshot](modules/runtime-interface.md#sessionsnapshot) |
| INV-201 | active Turn只使用admission时captured immutable Workspace/Prompt/Skill/Tool/Model对象；PromptContent在capture前已materialize，Turn执行不解析source locator | [Turn Execution Context · Context Capture](modules/turn-execution-context.md#context-capture) |
| INV-202 | exact Skill/Workspace authorization在composition前完成；每个contribution形成独立content part，live/JSONL只保留safe part-level provenance，replay不重新加载或授权source | [Prompt · PromptIntent和CanonicalUserMessage](modules/prompt.md#promptintent-和-canonicalusermessage) |
| INV-301 | Interaction live request在notify前apply并完成inline record attempt；resolution在resume前apply并完成inline record attempt | [Turn / Item / Interaction · Interaction Ordering](modules/turn-item-interaction.md#interaction-ordering) |
| INV-302 | MVP UserQuestion只有non-secret Text/SingleChoice；request/answer可进入JSONL/event/ToolResult/model，secret input必须走未来独立secure host capability | [Turn / Item / Interaction · UserQuestion](modules/turn-item-interaction.md#userquestion) |
| INV-401 | Tool side-effect start由ToolStartGate与EmergencyControl first-wins；Running后只能truthful settle | [Turn / Item / Interaction · Tool Side-Effect Start](modules/turn-item-interaction.md#tool-side-effect-start) |

## 模块地图

- [Wire Schema与Bounded Decode](modules/wire-schema.md)：public/storage JSON v1、typed scalar carrier、ProtocolLimits、canonical dynamic JSON和bounded scanner。
- [Wire V1 Conformance Fixtures](fixtures/wire-v1/README.md)：public manifest、golden/corruption vectors、boundary recipes与structural verifier。
- [Runtime公开协议](modules/runtime-interface.md)：dispatch/query/snapshot/subscribe和live observer语义。
- [Agent与Session生命周期](modules/agent-session-lifecycle.md)：definition、revision、load/unload/archive/fork。
- [Workspace](modules/workspace.md)：Session-owned Workspace、authority和immutable snapshot。
- [Prompt](modules/prompt.md)：materialized PromptContent、orthogonal PromptIntent、safe part-level contribution provenance和live model context assembly。
- [Skills](modules/skills.md)：SkillService、SkillView和reload。
- [Tools](modules/tools.md)：ToolSet、policy、approval、sandbox和executor。
- [Turn执行上下文](modules/turn-execution-context.md)：immutable capture和ConversationRevision basis。
- [Turn / Item / Interaction](modules/turn-item-interaction.md)：live lifecycle、Tool exchange和Interaction。
- [Conversation JSONL Format V1](formats/conversation-jsonl-v1.md)：exact Stored DTO envelope、field/tag、limits和corruption behavior。
- [Durable Store V1](formats/durable-store-v1.md)：exact local entity layout、generation/head bytes、markers、scanner/recovery precedence。
- [Durable Store V1 Fixtures](fixtures/durable-store-v1/README.md)：head/definition golden、closed crash taxonomy与structural verifier。
- [DurableState](modules/durable-state.md)：private store actor、reservation/lease/CAS/publication/recovery/fault seam。
- [Conversation Recording与Replay](modules/conversation-storage.md)：JSONL recorder、recording health、tolerant replay和fork。
- [Session执行](modules/session-execution.md)：control actor、ActiveTurnTask、async loop和queues。
- [ModelGateway](modules/model-gateway.md)：single provider attempt和response taxonomy。
- [Compaction](modules/compaction.md)：live rolling summary与best-effort marker。

## 相关决策

核心当前决策：

- [ADR 0137：Tokio owner-tracked async foundation与deterministic persistent seams](adr/0137-tokio-owner-tracked-async-foundation.md)
- [ADR 0136：DurableState使用operation-owned immutable generations、permanent reservations与root lease](adr/0136-durablestate-operation-owned-generations.md)
- [ADR 0134：Public Protocol与Conversation Recording使用bounded v1 wire schema](adr/0134-public-and-conversation-wire-use-bounded-v1-schemas.md)
- [ADR 0133：Runtime public payload必须可从Snapshot恢复并安全操作](adr/0133-runtime-public-payload-is-snapshot-recoverable.md)
- [ADR 0132：Compaction从revision-bound stable units派生marker](adr/0132-compaction-derives-markers-from-live-stable-units.md)
- [ADR 0131：Conversation Recording不保存Session definition/lifecycle](adr/0131-conversation-recording-excludes-session-definition-and-lifecycle.md)
- [ADR 0130：用户消息composition异步解析captured Skill](adr/0130-user-message-composition-resolves-skills-asynchronously.md)
- [ADR 0129：用户消息贡献使用part-level安全provenance](adr/0129-user-message-contributions-use-part-level-safe-provenance.md)
- [ADR 0128：Prompt content在publication前materialize](adr/0128-prompt-content-is-materialized-before-publication.md)
- [ADR 0127：Session recording不保存Turn lifecycle](adr/0127-session-recording-omits-turn-lifecycle.md)
- [ADR 0126：Turn执行使用async loop，Session记录采用inline best-effort append](adr/0126-turn-execution-is-async-and-session-recording-is-best-effort.md)
- [ADR 0125：ModelGateway不设置本地模型调用Permit](adr/0125-model-gateway-has-no-local-call-permits.md)
- [ADR 0124：Session replay宽容恢复](adr/0124-session-replay-is-tolerant-and-links-are-minimal.md)，其strict writer/committed-delta条款已被ADR 0126取代
- [ADR 0123：Exact Ref、immutable capture与explicit reload](adr/0123-identity-uses-refs-and-explicit-reload.md)，其durable checkpoint条款已被ADR 0126取代
- [ADR 0119：Session logical retry](adr/0119-model-calls-use-session-logical-retries.md)，owner已改为ActiveTurnTask
- [ADR 0118：Cancel立即确认并等待settlement](adr/0118-cancel-acknowledges-immediately-and-followup-waits-for-settlement.md)
- [ADR 0116：文件mutation使用Session-local queue](adr/0116-file-mutations-use-session-local-queues.md)

ADR 0104的durable truth/commit barrier、ADR 0105的single mutable Executor owner和ADR 0115的同步AgentLoop已被ADR 0126取代。
