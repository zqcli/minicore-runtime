# MiniCore V2 设计评审（第四轮）

状态：Open；Rust internal spine所需P0与P1-1/P1-2/P1-4已关闭，剩余P1-3/C0-1分别门禁production ProviderAdapter与Tool/Sandbox adapter
日期：2026-07-31
范围：全部current、非归档V2文档，包括`README.md`、`CONTEXT.md`、`docs/architecture.md`、module设计、ADR 0100–0134、migration、research和前三轮review/handoff
方式：主审逐份通读并交叉核对canonical owner；subagent结论仅作查漏线索，本文finding均由主审在current文档中独立复现

关闭进度：V4-P0-1至P0-5、V4-P1-1、V4-P1-2与V4-P1-4 Closed；V4-P1-3和V4-C0-1 Open。ADR 0130–0134是本review关闭过程中新增的Accepted决议。

## 总体结论

ADR 0126–0134确定的主方向继续成立：

```text
SessionExecutor control actor
└─ one ActiveTurnTask
   └─ async Model → Tool/Interaction → Model loop

LiveSessionState = current-process truth
SessionRecorder = inline best-effort conversation recording
Rig = ModelGateway private ProviderAdapter
```

本轮没有发现需要恢复同步AgentLoop、SessionWriter durable commit barrier、Turn lifecycle JSONL、PromptContent resolver或字符offset provenance的理由。

本review创建时识别出的五个P0现已全部关闭。V4-P1-1/2/4也已关闭，剩余P1只在production provider scope；Sandbox enforcement继续作为首个production Tool/Sandbox adapter前的条件性P0。

严重度定义：

- **P0**：对应模块无法形成唯一Rust contract，或当前形状会破坏single owner、安全门禁、recording prefix或replay正确性；在实现该vertical slice前必须关闭。
- **P1**：内部spine可以先行，但public/storage/provider contract tests无法冻结，或文档测试会要求相反行为；在相关crate surface或production adapter冻结前必须关闭。
- **条件性P0**：当前ScriptedProviderAdapter/internal spine可继续；一旦开始production OS/network/process Tool adapter即升级为P0。

## Finding总览

| ID | 严重度 | 状态 | Finding | 关闭门槛 |
| --- | --- | --- | --- | --- |
| V4-P0-1 | P0 | Closed | `ModelCallRequest`有两套不兼容定义 | ordinary AgentRun前 |
| V4-P0-2 | P0 | Closed | Tool call/outcome/start state与mutation queue contract未统一 | complete Tool exchange前 |
| V4-P0-3 | P0 | Closed | Prompt/Skill composition的sync/async与capture ownership无法闭合 | Submit/Steer Skill path前 |
| V4-P0-4 | P0 | Closed | conversation JSONL仍含无合法single-producer owner的Session definition/lifecycle events | storage schema前 |
| V4-P0-5 | P0 | Closed | Compaction source无法产生`first_kept_entry_id`，settings/provenance schema也未闭合 | Compaction vertical slice前 |
| V4-P1-1 | P1 | Closed | Runtime public protocol缺少可恢复、可操作的完整payload | public protocol crate前 |
| V4-P1-2 | P1 | Closed | 通用wire/storage envelope与限制未冻结 | serde fixture与format v1前 |
| V4-P1-3 | P1 | Open | Provider首版scope、Rig现实映射和旧permit/retry措辞未统一 | production ProviderAdapter前 |
| V4-P1-4 | P1 | Closed | Workspace Test Matrix要求与ADR 0116相反的跨Session文件锁 | Workspace/Tool tests前 |
| V4-C0-1 | 条件性P0 | Open | Sandbox不可强制capability时的pre-execution fail-closed门禁 | production Tool/Sandbox adapter前 |

## V4-P0-1 · `ModelCallRequest`有两套不兼容定义

状态：Closed（2026-07-31）。`docs/modules/model-gateway.md`现为唯一canonical owner；Turn Execution Context已删除第二份struct，ordinary/Structured/Compaction和logical retry统一使用同一个immutable request type。

### 关闭前场景

ordinary AgentRun完成Prompt assembly后创建唯一provider-neutral request。关闭前同时按Turn Execution Context和ModelGateway建模会得到两个不同struct：

- Turn Execution Context旧定义：
  - `context: Arc<AssembledModelContext>`；
  - 独立、必填`output_contract: OutputContract`；
  - `effective_max_output_tokens: u32`。
- ModelGateway定义：
  - `input: Arc<AssembledModelContext>`；
  - output contract只存在于`input.output_contract: Option<OutputContract>`；
  - `max_output_tokens: Option<NonZeroU32>`。

Prompt的`PromptAssemblyProof.output_contract`也定义为`Option<OutputContract>`。普通非structured AgentRun允许`None`，因此旧Turn Execution Context版本无法表示主路径。

### 影响

- Rust中无法选择唯一字段和constructor；
- caller与Gateway可能分别校验两份output contract，形成第二事实源；
- `None`、model default和明确max output三种语义会被压成一个裸`u32`；
- logical retry无法保证复用的是同一种immutable request；
- ordinary AgentRun、Structured和CompactionSummary contract tests会产生不同预期。

### 已采纳决议

以ModelGateway为`ModelCallRequest`唯一canonical owner：

```rust
pub struct ModelCallRequest {
    model: Arc<TurnModelSnapshot>,
    purpose: ModelCallPurpose,
    input: Arc<AssembledModelContext>,
    source_revision: ConversationRevision,
    max_output_tokens: Option<NonZeroU32>,
}
```

Turn Execution Context只说明它如何调用private constructor并链接ModelGateway，不再复制struct。OutputContract只保存在`AssembledModelContext`及其private proof中；request constructor验证proof，不保存第二字段。

### 关闭验证

- current modules只剩一个完整struct定义；
- ordinary AgentRun `output_contract = None`有fixture；
- NoToolCalls、Structured和CompactionSummary的constructor matrix使用同一类型；
- logical retry property test复用exact same `Arc<ModelCallRequest>`。

## V4-P0-2 · Tool contract未统一

状态：Closed（2026-07-31）。Tools现为ToolCall/Request/Outcome唯一owner，Session Execution拥有唯一ToolOperationSlot与SessionFileMutationQueue，Turn/Item只保留投影和complete exchange；所有pre-execution exact outcome统一生成truthful ToolResult。

### 关闭前场景

模型返回一个schema-invalid call、approval deny或Cancel-before-start。ToolSet必须产生truthful pre-execution ToolResult，使complete exchange可以闭合。关闭前modules给出互斥表示：

- Tools旧定义：
  - `ToolCall { id, name, arguments, index }`；
  - `ToolExecutionOutcome::Completed { item_id, call_id, source, result } | Abandoned { ... }`。
- Turn/Item旧ToolCall定义：
  - `ToolCall { item_id, tool_call_id, name, arguments }`。
- Turn/Item旧outcome定义：
  - `Completed(ToolResult) | PreExecutionFailed(...) | CancelledBeforeStart | Abandoned(...)`。
- Tools原有pre-execution规则明确deny/failure/cancel返回`Completed { source = PreExecution, result.disposition = Failed | Denied | Cancelled }`。

第二套outcome把pre-execution failure与cancel表达成无`ToolResult`的独立variant，无法直接满足`INV-003`“全部matching truthful ToolResult后才model-visible”。

关闭前邻接状态也未闭合：

- Turn/Item定义`ToolOperationState`，Session Execution定义另一套`ToolOperationSlot`；
- ADR 0116要求SessionExecutor拥有`SessionFileMutationQueue`并让ToolSet持共享引用；
- `SessionExecutor`和`ToolTurnContext`都没有mutation queue注入点；
- `ToolTurnContext.execution_control`写为由ActiveTurnTask注入，但ToolSet在ActiveTurnTask spawn前的Turn capture期间构造。

### 影响

- ActiveTurnTask无法对ToolSet返回值写出唯一match；
- pre-execution deny/cancel可能导致exchange永久incomplete，或被迫在上层合成未定义ToolResult；
- Item、Tool call index、ToolCallId与ItemId映射可能出现两份对象；
- Tool start/Cancellation的first-wins状态会在两个enum中漂移；
- Session-local mutation FIFO无法接入真实ToolSet，ADR 0116无法实施。

### 已采纳决议

1. Tools拥有唯一execution input/output类型；Turn/Item模块只消费并投影，不重新定义。
2. pre-execution所有exact outcome统一返回`Completed { source = PreExecution, result }`；`Abandoned`只表示已经无法得到truthful ToolResult的outcome unknown。
3. `ToolExecutionRequest`继续单独携带`item_id`，canonical `ToolCall`保存`ToolCallId + name + arguments + call_index`；Turn/Item projection把ItemId与call关联。
4. Session Execution拥有唯一`ToolOperationSlot`；Turn/Item只投影`Started | Completed | Abandoned` Item状态。
5. SessionExecutor显式拥有`Arc<SessionFileMutationQueue>`。ToolSet capture通过private `ToolTurnContext`取得queue和actor-owned `ToolExecutionControl` handle，或在`execute()`时接收一个统一execution context；不得依赖ActiveTurnTask尚未存在时“由task注入”。
6. `ToolExecutionError`只作为Tools内部分类，并明确映射到PreExecution ToolResult、Executed ToolResult或Abandoned，不能与`ToolExecutionOutcome`并列成为caller必须二选一的terminal事实。

### 关闭验证

- ToolCall、ToolExecutionOutcome和ToolOperationSlot各只有一个canonical owner；
- unknown/schema-invalid/approval-deny/cancel-before-start均生成matching truthful ToolResult；
- complete exchange fixture无需上层synthetic conversion；
- Session-local same-file FIFO通过真实ToolSet调用路径运行；
- ToolSet capture发生在task spawn前时，control/queue handle来源仍可构造。

## V4-P0-3 · Prompt/Skill composition的async seam无法闭合

状态：Closed（2026-07-31；ADR 0130）。TurnExecutionContext现绑定SkillService/SkillViewContext/SkillView并提供唯一async `resolve_user_message()`；PromptSet同步normalize，Session Execution拥有Starting/Steer cancellation与await后basis重验。

### 关闭前场景

Submit或Steer携带SkillIntent。TurnExecutionContext需要从本Turn的SkillView加载captured bytes，再由SkillInjector产生PromptContribution。

关闭前interface冲突：

- Turn Execution Context把`compose_input()`和`compose_steer()`定义为同步`fn`；
- Skills把`SkillService::load()`定义为`async fn`；
- Context只捕获`Arc<SkillView>`，没有SkillViewContext或SkillService/loader；
- Skills又要求TurnExecutionContext同时捕获SkillViewContext并调用SkillService；
- `SkillView`本身只含entries，没有绑定load所需context；
- Skills cache段落仍要求已被ADR 0126删除的`execution_version/current operation`校验。

Starting阶段还要求`Cancel(Submit CommandId)`持续有效。任何async Skill load/candidate composition都必须观察已发布的candidate emergency target，并且不得持有live-state或lifecycle guard跨await。

### 影响

- 关闭前方法签名无法调用SkillService；
- Steer无法按Turn-pinned旧Skill bytes展开；
- 实现者可能在PromptSet中偷偷做I/O，破坏唯一assembly seam；
- 实现者也可能重新读取current Skill source，破坏explicit reload和INV-201/202；
- Starting async等待若没有candidate control seam，会形成按CommandId无法及时停止或无法安全丢弃迟到composition result的窗口。

### 已采纳决议

采用保留Skill lazy parse并拆成async resolve + sync normalize的方案：

```text
TurnExecutionContext.resolve_user_message(intent).await
→ 使用captured SkillViewContext + bound Skill loader加载全部Skill
→ SkillInjector产生typed contributions
→ 构造private UserMessageCompositionInput
→ PromptSet.compose_user_message()同步纯内存规范化
```

TurnExecutionContext捕获同一个`Arc<SkillService>`、`Arc<SkillViewContext>`和绑定该context的`Arc<SkillView>`。SkillService load接收captured view与其entry，PromptSet继续不持有SkillService。

Starting candidate在await前安装CommandId target与observed emergency epoch。control actor的Starting subloop同时等待async resolve、out-of-band EmergencyControl与Lifecycle；Cancel/SecurityRevoked先赢时drop future。resolve返回后必须重验candidate target/control generation/authority，再允许live Input apply。删除Skills文档中的`execution_version/current operation`，统一为current Turn/candidate target、control_generation、ConversationRevision和captured view validation。

同步eager parse方案未采用；lazy async parse与Prompt sync normalize的owner保持分离。

### 关闭验证

- Submit和Steer各有唯一、可调用的Skill composition路径；
- Context字段与Skills owner文档一致；
- composition await前后不持有live-state/Agent/Session lifecycle guard；
- Cancel/SecurityRevoked在Starting load期间可立即发布并阻止live apply或task spawn；
- reload-during-Steer fixture证明只解析old captured bytes。

## V4-P0-4 · conversation JSONL含无合法writer owner的configuration/lifecycle events

状态：Closed（2026-07-31；ADR 0131/0134）。Format v1使用六种flat StoredEntryBody conversation facts，不再定义StoredEvent wrapper；Agent/Session definition、metadata和lifecycle由entity durable owner保存，Create/Archive/Delete与loaded update均不调用Recorder。

### 关闭前场景

关闭前Conversation Storage定义：

```rust
StoredEvent::SessionDefinitionChanged(...)
StoredEvent::SessionLifecycleChanged(...)
```

关闭前ownership无法写入这些variants：

- ADR 0127第2条明确Session/Agent definition由各自durable owner保存；
- SessionRecorder只在loaded Session存在，Create发布`Open + Unloaded`时不创建Recorder；
- Archive/Delete等lifecycle transition要求Unloaded；
- unloaded Session也允许definition/metadata mutation；
- loaded Session的Model/Prompt future-only definition update可以在active Turn期间提交（`agent-session-lifecycle.md:697-713`）；
- Recorder single-producer规则（`conversation-storage.md:181-187`）规定Running recordable mutation由ActiveTurnTask拥有，不允许另一个lifecycle producer靠Recorder mutex竞争排序；
- EntryId只能由loaded `LiveSessionState.EntryIdGenerator`分配，unloaded lifecycle owner没有该generator。

### 影响

保留这些variants会迫使实现选择下列任一错误路径：

- unloaded durable owner直接写conversation JSONL，绕过EntryId owner和SessionRecorder；
- loaded definition update与ActiveTurnTask成为两个record producer，mutex获取顺序定义history；
- 只在部分状态记录，形成无法解释的不完整configuration timeline；
- Fork复制source configuration/lifecycle events到child conversation tree，产生source/child ownership歧义。

### 已采纳决议

MVP从conversation JSONL删除`SessionDefinitionChanged`和`SessionLifecycleChanged`。Agent/Session durable owner保存current head/revision/lifecycle；Runtime query/snapshot/typed event surface负责current观察，metadata专用event仍由V4-P1-1冻结。Conversation JSONL只保留：

```text
User / Assistant / Tool conversation messages
Interaction request/resolution historical facts
StoredCompaction
```

若未来需要configuration audit，应建立独立durable audit/definition history seam，使用该owner自己的revision和原子store，不借用LiveSessionState EntryId tree。

### 关闭验证

- Format v1不存在StoredEvent wrapper或任何configuration/lifecycle body variant；
- 所有StoredSessionEntry都有唯一EntryId分配owner；
- active Turn期间definition update不会成为第二record producer；
- Fork只复制conversation/history facts，不复制source lifecycle timeline。

## V4-P0-5 · Compaction source/settings/provenance contract未闭合

状态：Closed（2026-07-31）。ADR 0132冻结reducer-owned revision-bound stable units、Runtime-global Turn-captured settings、完整Pressure/Plan input、source+cut marker derivation和automatic model-call provenance。

### 关闭前场景

Compaction planner从sanitized live conversation选择prefix cut，并保存summary后的第一个exact retained entry：

```rust
first_kept_entry_id: Option<EntryId>
```

关闭前source无法提供该值：

- `docs/modules/compaction.md:95-105`要求plan包含`first_kept_entry_id`；
- `docs/modules/compaction.md:109-115`声明source只来自`LiveConversationView`；
- `docs/modules/conversation-storage.md:457-460`和`turn-execution-context.md:134-137`的LiveConversationView只有`ConversationRevision + Arc<[ModelMessage]>`，没有EntryId、stable unit或message-origin映射；
- 文本完全相同的多条消息不能靠value equality安全反查EntryId；
- complete Tool exchange跨多条entry，marker必须落在整个protocol unit边界。

同一module还有两个未闭合输入：

- `TurnExecutionContext.compaction: CompactionSettingsSnapshot`只在`turn-execution-context.md:74`出现，没有type、字段、默认值或capture source；`compaction.md:270`明确仍需freeze；
- `StoredCompactionModelCall`当前只列`model + usage + logical_retry_count`（`compaction.md:189-193`），而ModelGateway要求automatic path还保存finish、requested max output和allowlisted provider metadata（`model-gateway.md:1584`）；ADR 0123第17条也要求safe finish provenance。

### 影响

- live planner无法构造可被cold replay应用的marker；
- 实现者可能按message equality猜EntryId，重复消息时压缩错误prefix；
- ToolCall/ToolResult unit可能被marker切开，replay只得忽略Compaction；
- settings来源不同会使同一Turn的trigger、budget和retry上限漂移；
- StoredCompaction encoder/decoder与ModelGateway无法共享唯一schema。

### 已采纳决议

1. LiveConversation提供private compaction source projection，按provider-valid stable unit携带exact origin：

```rust
struct LiveCompactionUnit {
    first_entry_id: EntryId,
    messages: Arc<[ModelMessage]>,
    kind: CompactionUnitKind,
}

struct LiveCompactionSourceView {
    session_id: SessionId,
    revision: ConversationRevision,
    units: Arc<[LiveCompactionUnit]>,
}
```

complete Tool exchange是一个unit；rolling summary unit的origin是对应StoredCompaction outer entry。Compaction从`source + summarized_unit_count`派生marker，不从ModelMessage反查。review草案中的`estimated_tokens`没有进入reducer view：estimate改由Compaction使用Turn-pinned TokenEstimator计算并保存在plan proof中，避免conversation projection变成model-specific state。

2. `CompactionSettings`是validated Runtime startup config，Turn admission捕获immutable snapshot。默认值冻结为enabled=true、pressure reserve=4096、summary min/max=512/2048、minimum reclaimed=2048、max Compactions per Turn=4、summary safety reserve=512。
3. `CompactionPressureInput`与`CompactionPlanInput`完整携带exact source、captured settings、Prompt AgentRun/Summary bases、Turn model basis、trigger和per-Turn count；cut同时验证summary-call budget、post-Replace headroom和minimum reclaim。
4. Compaction唯一拥有`StoredCompactionModelCall` semantic schema：model、response ID、usage、finish reason、requested max output、logical retry count和allowlisted provider metadata。automatic path总是`Some`。
5. 新增INV-005：apply要求exact Session/control/plan/request/revision，LiveSessionState在record前分配Compaction EntryId并安装new rolling-summary origin；cold replay只接受stable-unit first-entry marker。权威决议见[ADR 0132](../adr/0132-compaction-derives-markers-from-live-stable-units.md)与[Compaction](../modules/compaction.md)。

### 关闭验证

- [x] 重复文本消息fixture按不同EntryId产生exact marker；
- [x] complete Tool exchange是不可拆unit，marker不能指向ToolResult；
- [x] settings从Runtime config到Turn snapshot只有一条capture路径；
- [x] automatic StoredCompaction `model_call = Some`且semantic fields与ModelGateway一致；
- [x] marker missing/orphan/ignored/first-unit与later-valid replay fixtures进入canonical Test Matrix。

## V4-P1-1 · Runtime public protocol缺少可恢复、可操作的完整payload

状态：Closed（2026-07-31）。[ADR 0133](../adr/0133-runtime-public-payload-is-snapshot-recoverable.md)冻结snapshot-recoverable closed payload、metadata CAS、SubmitCancelled completion、public error mapping、安全Interaction与MVP Prompt body。canonical owner见[Runtime Interface](../modules/runtime-interface.md)、[Agent/Session Lifecycle](../modules/agent-session-lifecycle.md)、[Tools](../modules/tools.md)、[Turn / Item / Interaction](../modules/turn-item-interaction.md)和[Prompt](../modules/prompt.md)。

关闭映射：

- `df0dce8`：Agent/Session独立metadata revision、CAS、outcome、event与readback；
- `3687be4`：exact-one CommandCompletion、Starting Submit cancel竞态和public error mapping；
- `8889601`：SessionSnapshot完整枚举可取消Submit/Steer/FollowUp CommandId，新增INV-103；
- `f709ba2`：concrete QueryResult、Runtime/Session Snapshot、Item/terminal/usage/diagnostic safe read models；
- `c29268a`：request-scoped approval option、non-secret UserQuestion、Interaction resolution key、safe Runtime/event payload与storage secret boundary、INV-302；
- `d260117`：删除未定义Template intent/query/decode入口，MVP只保留Empty/Text；
- `6961403`：定义全部nested Command/Query request DTO、typed catalog args/cursor/suggestion和ReasoningPreference；
- `b364461`：Runtime durable entity event携带changed Agent/Session summary，闭合metadata event token；
- `0c4a0ce`：严格分离outer dispatch failure与accepted-command error，并冻结SessionNotReady cause→retry映射；
- `4fee962`：Workspace public update只CAS outer SessionDefinitionRevision，删除第二个WorkspaceRevision proof；
- `d5ae19a`：补齐UserMessageSource、AssistantDisposition和ToolResultDisposition closed Item semantics。

因此重新subscribe后的host可以只用Snapshot恢复全部current public cancel/resolve action；secret input fail closed；metadata和Submit completion可进行typed contract test；public enums无未定义future payload。exact serde casing、ID/Timestamp/Money与byte/count limits现由ADR 0134、Wire Schema和Format V1冻结。

### 关闭前问题

该组问题不阻塞最小internal reducer，但会阻塞public protocol crate、snapshot-first host和contract tests。

### Queue可操作性

`CancelQueuedMessage`要求`target_command_id`（`runtime-interface.md:402-405`），而`SessionQueueView`只有三个count和`accepting_input`（`905-910`）。`queue_updated`事件携带removed IDs，但Event不重放。host断线后从Snapshot无法枚举仍存在的Steer/FollowUp，也无法调用CancelQueuedMessage。

推荐：Snapshot增加bounded UI-safe queued message列表，至少包含`CommandId + Steer/FollowUp kind + target TurnId(optional) + redacted preview`；或删除public CancelQueuedMessage。只保留count同时承诺snapshot-first完整恢复不可闭合。

### Interaction payload与secret policy

`InteractionRequestView`仍是注释占位（`runtime-interface.md:1277-1280`），ToolApproval/UserQuestion的展示字段、answer shape、size limit和redaction均未定义。`resolution_key: IdempotencyKey`的生成方、随机性、scope和retry复用规则也未冻结。

当前Interaction resolution会进入JSONL和StateEvent。若UserQuestion可收集password/token，answer可能进入普通recording、snapshot或diagnostic。MVP必须二选一：

- 明确UserQuestion只接受non-secret输入，credential/auth输入走独立host安全能力；
- 或增加secret typed field，并规定secret answer只存在于live waiter，recording/event只保存redacted resolution status。

ToolApproval view至少冻结tool name、redacted argument summary、requirements摘要和decision family；不得携带prepared private args、executor route或Sandbox internals。

### Metadata CAS没有read/write闭环

`UpdateMetadata`使用`AgentMetadataRevision`/`SessionMetadataRevision`（`runtime-interface.md:254-257`、`292-295`），但Agent/Session durable head没有对应字段，current modules也没有定义这两个type。lifecycle文档一处仍写metadata update与definition update都CAS expected AgentRevision（`agent-session-lifecycle.md:228`），后文又要求独立metadata version（`:713`、`:972`）。

`CommandOutcome::AgentUpdated`只返回AgentRevision，`SessionUpdated`只返回SessionDefinitionRevision（`runtime-interface.md:508-520`）；event kind也只有definition updated，没有metadata updated。metadata update不递增这些execution revisions，因此caller拿不到下一次CAS token。

推荐：在entity head定义独立opaque metadata revision；Create/UpdateMetadata outcome和Runtime event返回new metadata revision；definition update与metadata update使用不同event kind。

### Starting Submit被取消时缺少typed completion

Cancel可在Input apply前关闭pre-Turn Submit。`runtime-interface.md:573`要求原Submit返回`Rejected(Cancelled)`，但`CommandOutcome`没有Rejected variant，`CommandErrorCode`也没有Cancelled。必须冻结原Submit的typed completion，以及Input已apply后仍按设计返回TurnStarted再发布TurnInterrupted的分界。

### Error mapping与Prompt variant

- `SessionExecutionError/PromptError/ToolError → CommandErrorCode + RetryAdvice`没有canonical mapping，内部模块和public contract tests会各自解释Busy、NotReady、Cancelled、Unavailable和InvariantFailure。
- 关闭前public Prompt body保留了未定义的Template variant；Prompt后续问题仍在讨论模板是否属于MVP。首版应定义完整materialized template intent，或从MVP public enum移除该variant；关闭时采用后者。

### 关闭条件

- 重新subscribe后host能对所有public queue和Pending Interaction执行合法command；
- Interaction request/resolution semantic payload完整且secret policy fail closed；exact wire DTO/casing/limits由ADR 0134与Wire Schema冻结；
- metadata update有独立CAS token、outcome和event；
- pre-Turn Submit cancel有typed completion；
- public error mapping有contract table；
- public enum中不存在未定义future variant。

## V4-P1-2 · 通用wire/storage envelope与限制未冻结

状态：Closed（2026-07-31）。[ADR 0134](../adr/0134-public-and-conversation-wire-use-bounded-v1-schemas.md)与[Wire Schema](../modules/wire-schema.md)冻结public/storage JSON v1、typed scalar carriers、ProtocolLimits、bounded JSON和scanner；[Conversation JSONL Format V1](../formats/conversation-jsonl-v1.md)冻结Header、六种Stored body、field order、relation/replay与Compaction projection；[Wire V1 Fixtures](../fixtures/wire-v1/README.md)提供public manifest、golden/corruption vectors、all-limit recipes和structural verifier。

关闭映射：

- `e1f916f`：ADR 0134与bounded wire v1 foundation；
- `367ad25`：wire-reachable Prompt/Tool/Model/Interaction/usage/path semantic leaves；
- `cd6b5cb`：exact conversation JSONL format v1及全部consumer同步；
- `4a56937`：public/storage conformance vectors、bounded recipes和replay diagnostic/selection contracts；
- 本closure change：补齐capability-intersection negotiation cases并同步review/handoff/migration状态。

### 关闭前问题

关闭前modules显式保留以下开放项：

- `conversation-storage.md:579`：EntryId算法/文本wire、max entry bytes、diagnostic总量上限、format migration；
- `compaction.md:270`：StoredCompaction wire；
- `turn-execution-context.md:286`：serde casing和public ID格式；
- Runtime协议使用`Timestamp`、`Money`、ID、revision、Duration、JSON schema等共享值，但没有统一wire定义。

这组问题会阻塞serde derive、golden fixtures、tolerant decoder和跨版本replay。尤其需要在format v1前冻结：

1. field naming、enum tagging、unknown variant policy；
2. Session/Turn/Item/Request/Entry/Command/Skill等ID的文本格式、生成scope和长度；
3. UTC Timestamp精度与parse policy；
4. Money decimal/currency表示；
5. `max_entry_bytes`、单字段字符串/opaque artifact、Tool result和diagnostic数量/长度上限；
6. unknown format version、旧format migration和future field兼容规则；
7. `ModelResponseSummary`、`StoredToolOutcome`、StoredInteraction request/resolution等当前仅被引用但未定义的storage types；
8. partial tail与oversized complete line的decoder行为。

推荐建立一个小型wire/schema ADR或canonical基础类型module，不建设通用domain“Common”大杂烩。该决议只拥有serialization和bounded decode，不拥有各module业务语义。

### 已采纳决议

Wire Schema只拥有representation、bounded decode、canonical dynamic JSON和limits，不成为domain Common registry。v1固定：camelCase fields、snake_case variants、adjacent `type/data` tagging；MiniCore IDs使用typed prefix + 128-bit lowercase hex，revision/u64/Timestamp/Duration/Money/path/cursor均有exact carrier；client input fail closed，Runtime output selected-minor only，entry additive field tolerant但unknown fact skip；duplicate key全层拒绝。

Conversation JSONL首行strict Header，后续entry required SessionId/TurnId并使用六种flat body。Header 64 KiB、entry 1 MiB、file 1 GiB/1,000,000 entries；whole-file cap先于tail truncation。Replay以first canonical root + physical-last eligible leaf选择path，session mismatch不poison EntryId collision guard，Tool exchange/Interaction/Compaction关系按closed diagnostics隔离。public Snapshot/Query diagnostics按50/100 limits重新投影typed replay totals。

### 关闭条件

- [x] format v1有byte-exact golden JSONL和public protocol manifest/vectors；
- [x] oversized/malformed/unknown variant使用bounded scanner，不无界分配或brick later recoverable prefix；
- [x] writer与tolerant decoder共享同一tag/casing/ID/canonical dynamic JSON representation；
- [x] 全部Stored* semantic types有唯一module owner；Format V1只声明exact representation projection，non-owner module不复制semantic declaration；
- [x] Header/entry/file/count、ProtocolLimits、BoundedJson/Schema boundary/+1 recipes已冻结；
- [x] `python3 docs/fixtures/wire-v1/verify.py`、Markdown link/fence与`git diff --check`通过。

## V4-P1-3 · Provider首版scope、Rig映射和旧措辞未统一

### 首版scope

`ProviderProtocol`当前公开四个variants（`model-gateway.md:1381-1386`）：

```text
OpenAiResponses
OpenAiChatCompletions
AnthropicMessages
Gemini
```

实际production gate和mock-server计划只覆盖OpenAI Responses与Anthropic Messages。OpenAI Chat Completions/Gemini没有首版adapter、contract matrix或明确Unsupported行为。config若接受这些variant，会发布无法执行或未经验证的model definition。

推荐冻结首版支持集。最保守方案是首版enum只保留经contract test的protocol；也可以保留variant但candidate validation必须明确返回`UnsupportedProviderProtocol`，safe catalog不得把它显示为available。

### Rig spike

Rig 0.40.0 spike仍需验证：system/instructions、message/tool schema、Anthropic thinking/signature/cache control、stream cancellation、finish、usage、error delivery proof、SDK retry=0、base URL和mock server。Spike只允许调整private adapter/mapping或暴露真实缺失的provider-neutral字段，不能把Rig Agent/Conversation类型引入MiniCore seam。

### current residue

ModelGateway current文档仍有被ADR 0125/0126取代的现行措辞：

- `model-gateway.md:1225`、`:1241`要求取消“concurrency wait”，但当前Gateway没有本地permit/admission wait；
- `:1339`称AuthPrincipalIdentity用于“per-principal concurrency”，与Gateway禁止per-auth-principal permit的current规则冲突；
- `:1744`测试要求Steer使logical retry失效，而Session Execution canonical规则（`session-execution.md:296`）规定RetryBackoff期间Steer只排队且不改变revision，retry继续；Steer本来就不取消Sampling。

这些句子会让实现或测试重新引入本地permit，或在queued Steer时错误放弃安全retry。

### 关闭条件

- 首版protocol enum与实际adapter/test范围一致；
- Rig spike和两个mock-server contract tests通过；
- SDK retry确认为0；
- current modules不再出现Gateway-local concurrency wait/per-principal permit；
- queued Steer对in-flight result、retry backoff和safe-point consumption只有一套规则。

## V4-P1-4 · Workspace Test Matrix与跨Session规则相反

状态：Closed（2026-07-31）。Workspace Test Matrix现明确不同Session使用独立queue、互不等待；同Sessionalias仍按canonical `FileMutationKey`进入同一FIFO。

ADR 0116和Tools canonical规则明确：每个loaded Session拥有独立mutation queue；两个Session即使写同一physical file也不共享锁，可能并发，host/user负责worktree或外部隔离。

关闭前Workspace Test Matrix要求：

> 不同 Session 使用不同 root anchor 指向同一目标时仍竞争同一文件锁

这会让Workspace/Tool测试实现与ADR 0116、`tools.md:382`和`:573`相反。

已采纳测试规则：

```text
两个Session指向同一physical target
→ 使用不同SessionFileMutationQueue
→ 不互相等待
→ fixture明确展示可能并发/lost update
```

同一Session的symlink/absolute/relative alias仍必须canonicalize到同一`FileMutationKey`并竞争同一个queue。

### 关闭验证

- current Test Matrix只要求同Sessionalias归一后互斥；
- cross-Session fixture明确验证无协调；
- 文档不再暗示MiniCore提供跨Runtime/process文件隔离。

## V4-C0-1 · Sandbox enforcement条件性P0继续有效

这是第一轮O1、第二轮R7的同一问题，未被ADR 0126–0134关闭。

production Tool/Sandbox adapter必须在ToolStartGate前声明可强制的capability classes。最终PermissionSet要求与adapter enforceable capabilities的差集非空时，必须生成PreExecution Denied ToolResult并拒绝side effect。approval不能把不可强制限制转换为裸执行许可，Sandbox失败也不能静默fallback到无Sandbox执行。

该门禁不阻塞ScriptedProviderAdapter、纯内存Tool fixture或无真实OS/network/process副作用的测试实现；首个production adapter开始前必须升级为P0并接受ADR/canonical Tools更新。

## 未升级为Finding的事项

以下内容经主审复核后不作为第四轮P0/P1：

- ADR 0126之前的SessionWriter、committed delta、RunningOperation和同步AgentLoop问题；
- Recorder Q1–Q10；全部已关闭；
- Prompt Q1/Q4；已由ADR 0128/0129关闭；
- Projection snapshot/checkpoint index；MVP接受O(n) replay；
- repair utility；ADR 0124已采用tolerant replay；
- durable Turn status、restart closure和same-Turn resume；ADR 0127已明确删除；
- Agent/Session revision GC、physical purge、auto-unload、multi-process store；属于后续lifecycle/operations backlog；
- Prompt cache/hook、SkillScope/metadata扩展、Workspace remote backend、manual compaction；无首个vertical slice需求；
- Gateway本地模型permit；ADR 0125继续有效；
- README遗漏`query`、Agent/Session lifecycle局部“three services”措辞；属于导航问题，已在本轮机械修复，不改变canonical contract。

## 当前继续顺序

已完成：V4-P0-1 → P0-2/P1-4 → P0-3 → P0-4 → P0-5 → P1-1 → P1-2；Rust crate与typed scalar/value/path carriers已经建立。

后续以[MiniCore V2开发计划](../development-plan.md)为实施入口：M0文档收敛与质量门禁已经完成，当前暂停于M1.1前；恢复后再完成bounded public/storage codec、fixture runner、LiveConversation reducer和ScriptedProvider vertical slice。V4-P1-3继续门禁production ProviderAdapter；V4-C0-1继续门禁production Tool/Sandbox adapter。

## 第四轮关闭定义

本文全部P0/P1关闭需要：

- canonical owner文档已修订，非owner只保留摘要与链接；
- architecture INV索引仅在不变量语义变化时更新；
- Accepted ADR记录真正的新决策或supersession；
- migration/handoff同步实施顺序；
- current modules不存在重复不兼容共享类型；
- `rg`旧术语和冲突variant扫描无current residue；
- Markdown relative link、fence、INV owner/consumer和`git diff --check`通过；
- 每项closing fixture已列入对应module Test Matrix。

## 评审覆盖说明

主审已完整阅读全部current、非归档Markdown，并按下列切面交叉验证：

```text
Runtime facade / command / query / snapshot / event
Agent / Session durable lifecycle and loaded residency
SessionExecutor / ActiveTurnTask / Cancel / Steer / FollowUp
LiveConversation / SessionRecorder / replay / fork
Prompt / Skill / Workspace composition and provenance
Tool policy / approval / start / outcome / file mutation scheduling
ModelGateway request / provider / retry / usage / persistence
Compaction source / budget / marker / replay
ADR supersession / migration / research / prior reviews / handoff
```

本文finding只引用current canonical modules与仍有效Accepted ADR。archive/v1和已Superseded ADR正文未被当作现行合同。
