# Conversation Recording 与 Replay 架构设计

状态：当前权威架构（ADR 0134后，生产实现待启动）
日期：2026-07-31

## 目的

本文定义MiniCore的live conversation、SessionRecorder、by-entry JSONL、recorded history tree、tolerant replay、recording health、fork和restart recovery。

核心定位：

- `LiveSessionState`是loaded Session的current-process truth；
- `SessionRecorder`把live mutation顺序inline append为可恢复的best-effort前缀；
- append outcome不作为Model、Tool、Interaction或StateEvent的durable correctness proof；
- cold replay只恢复已经record的完整行；
- JSONL不记录Turn start/terminal、Session definition/metadata或Session lifecycle；
- replay仍必须构造provider-valid sanitized conversation。

本文不定义Model/Tool scheduling、Prompt assembly、Tool sandbox或Runtime wire schema。

## 同类项目基线

| 项目 | Live state | Recording |
| --- | --- | --- |
| Pi | Agent内存messages | `message_end`后同步JSONL append；listener可以先收到事件 |
| Codex | Session in-memory history | rollout recording，failure通常log-and-continue |
| Gemini CLI | agent history | finalized message同步append JSONL，ENOSPC可禁用recording后继续 |
| OpenHands | conversation state/event view | optional FileStore EventLog；未配置时使用InMemoryFileStore |

MiniCore采用同类产品的live-first语义，同时保留typed JSONL、tree identity、Tool exchange sanitizer和bounded diagnostics。

## 决策摘要

- 一个Session一个header + by-entry JSONL文件；
- Live reducer分配EntryId并先更新current-process state；
- `SessionRecorder::record(entry).await`顺序encode并append当前JSONL line；
- Recorder不使用后台task、channel或process-local queue；
- encode或physical write失败不回滚live state；
- first recording failure后停止该loaded Session后续记录；
- 文件最多是recordable conversation facts的一个合法前缀；
- Agent/Session durable configuration与lifecycle由entity owner保存，不进入conversation EntryId tree；
- cold replay宽容skip/isolate局部坏记录；
- incomplete/orphan/abandoned-first Tool exchange不进入模型conversation；
- EntryId继续用于recorded tree、fork和history query；
- `ConversationRevision`取代EntryId checkpoint作为live execution basis；
- TurnId只承担conversation correlation，不表达durable TurnStatus；
- restart不恢复未record tail、旧execution objects或旧TurnStatus。

## Ownership

```text
SessionExecutor / ActiveTurnTask
├─ LiveSessionState
│  ├─ LiveConversation
│  ├─ Turn/Item/Interaction view
│  ├─ ConversationRevision
│  └─ EntryIdGenerator
└─ SessionRecorder
   ├─ single-Session async append serialization
   ├─ JSONL encoder
   ├─ exclusive recorder lease when available
   └─ RecordingHealth

SessionStorage
├─ create/open recorded file
├─ tolerant replay
├─ history tree/query
└─ stage recorded-path or live-snapshot Fork
```

Live reducer拥有domain validation、EntryId allocation和当前进程conversation。Recorder只接受private constructors创建的immutable `StoredSessionEntry`，不重新决定Model/Tool协议或identity。

## EntryId Ownership

```rust
pub(crate) struct EntryIdGenerator {
    // private algorithm + collision guard
}

impl EntryIdGenerator {
    pub fn allocate(&mut self) -> EntryId;
}
```

`EntryIdGenerator`由`LiveSessionState`私有持有，不暴露给SessionRecorder、Tool、ModelGateway、Prompt或Runtime adapter。owner规则：

- domain validation成功后、live apply之前分配EntryId；
- ordinary mutation的`parent_id`同时绑定当前live selected head；
- generated ID立即登记到collision guard，同一loaded instance内不复用；
- replay初始化时用文件中全部first-valid EntryId seed guard，不只使用selected model-visible path；
- loaded Fork child用已经materialize到target的全部copied EntryId seed guard，future entry分配fresh ID；
- Degraded时generator继续工作，recording failure不改变已经进入live Item/Interaction/Compaction的identity；
- EntryId不从JSONL line number、storage ordinal或ConversationRevision派生。

EntryId exact wire由[Wire Schema typed carrier](wire-schema.md#runtime-generated-ids)冻结为`ent_<32 lowercase hex>`；generator使用16 CSPRNG bytes且不承载时间/顺序。Recorder只验证entry identity已经存在且relation合法，不能创建、替换或规范化EntryId。

## Live Mutation And Recording

**Canonical cross-module invariant: INV-001.**

```text
validate recordable conversation mutation
→ LiveSessionState.EntryIdGenerator.allocate + bind parent_id
→ apply LiveSessionState
→ increment ConversationRevision when model-visible
→ await SessionRecorder.record(entry)
→ publish final StateEvent / resume waiter / continue protocol
```

`record().await`返回：

```rust
pub(crate) enum RecordOutcome {
    Written,
    NotRecorded { health: RecordingHealth },
}
```

`Written`只表示该JSONL line的`write_all`成功，不表示flush、fsync或power-loss durability。调用方不能把outcome当成execution permit，也不能因`NotRecorded`重新执行Model或Tool。

recordable mutation失败时不record、不publish。mutation成功后recording失败不回滚。record await期间不得持有`LiveSessionState` guard。Turn terminal state mutation没有对应`StoredSessionEntry`：它只apply live并发布StateEvent，不经过Recorder。

## ConversationRevision

```rust
pub(crate) struct ConversationRevision(u64);
```

规则：

- loaded Session初始化时从0开始；
- 每次model-visible live mutation递增；
- Input/Steer UserMessage、Assistant、complete Tool exchange和Compaction Replace会改变revision；
- progress、Interaction、usage-only或recording health变化不改变model conversation revision；
- ModelCallRequest和CompactionPlan捕获exact revision；
- revision是process-local执行basis，不写入JSONL，不跨restart比较。

EntryId继续存在，但只承担history identity和引用。ConversationRevision承担process-local model-visible basis；两者不能互相派生。

## SessionRecorder

```rust
pub(crate) struct SessionRecorder {
    session_id: SessionId,
    state: tokio::sync::Mutex<SessionRecorderState>,
    health: ArcSwap<RecordingHealth>,
}

impl SessionRecorder {
    pub async fn record(
        &self,
        entry: Arc<StoredSessionEntry>,
    ) -> RecordOutcome;

    pub fn health(&self) -> Arc<RecordingHealth>;
}
```

```rust
pub enum RecordingHealth {
    Healthy,
    Degraded {
        failed_entry_id: Option<EntryId>,
        reason: SessionRecordingError,
    },
}
```

该类型是crate-private Recorder状态。Runtime公开投影固定为`SessionRecordingView { state: Healthy | Degraded }`：

- internal reason和failed EntryId不进入普通Snapshot；
- Runtime把reason映射为allowlisted recording diagnostic code；
- 每次Load都尝试初始化Recorder；初始化、lease、权限、磁盘、encode或write failure进入`Degraded`；
- 同一loaded Session只允许`Healthy → Degraded`，Recorder不会在当前load中自动恢复Healthy；
- Degraded后不自动probe/retry、不创建新segment、不保留或backfill unrecorded suffix；
- Health与当前recording diagnostic通过同一次immutable Snapshot publication对host可见。

### 顺序与Prefix规则

MVP的recordable domain mutation保持单producer ownership：Starting/Idle mutation由SessionExecutor拥有，Running Turn mutation由ActiveTurnTask拥有，Interaction routing只在对应task等待期间完成request/resolution mutation。不得让两个独立caller并发apply可记录的live mutation，再依赖Recorder mutex acquisition决定history顺序。未来出现真实multi-producer需求时必须引入显式domain sequencing，不能静默依赖scheduler顺序。

Recorder的async mutex只保护单文件handle和append原子串行化。`health`通过独立immutable publication读取，Snapshot不等待当前file write。Recorder按单Session顺序处理当前entry。以下任一情况首次发生后进入Degraded并停止后续append：

- encode失败；
- lease丢失；
- create/append I/O失败；
- partial write或outcome unknown。

因此不会在已知丢失entry之后继续写后续entry。process crash或failed write仍可能留下最后partial line；replay忽略该tail，只恢复此前newline-terminated的有效前缀。Recorder不执行per-entry `fsync`或`sync_data`。

### Recording failure

recording failure：

- 发布redacted diagnostic/health update；
- 不使Session execution Unavailable；
- 不阻止Submit、Model、Tool、Interaction、Compaction或live terminal settlement/publication；
- 不自动切换到第二文件或新segment；
- 不尝试追溯补写已经unrecorded的live gap；
- 存储恢复后当前loaded Session仍保持Degraded，后续`record()`继续返回`NotRecorded`。

Recorder没有后台queue、flush watermark或drain operation。graceful unload等待或取消ActiveTurnTask；task结束后不存在待写record tail。Host可以继续unsaved execution，或显式`Unload + Load`从recorded prefix创建新的loaded instance。显式Fork在source已loaded时使用LiveSnapshot，因此可以把snapshot捕获前已apply的unrecorded tail写入独立child record stream。

## SessionStorage Interface

当前只有一个backend，不建立public adapter hierarchy。

```rust
pub(crate) struct SessionStorage {
    // root/index/config
}

impl SessionStorage {
    pub async fn stage_create(
        &self,
        request: CreateSessionStorage,
    ) -> Result<StagedSessionStorage, SessionStorageError>;

    pub async fn open(
        &self,
        session_id: SessionId,
    ) -> Result<OpenedSession, SessionStorageError>;

    pub async fn replay_at(
        &self,
        session_id: SessionId,
        entry_id: EntryId,
    ) -> Result<ReplayedSession, SessionStorageError>;

    pub async fn fork_recorded(
        &self,
        request: ForkRecordedSession,
    ) -> Result<StagedSessionStorage, SessionStorageError>;

    pub async fn fork_live(
        &self,
        request: ForkLiveSession,
    ) -> Result<StagedSessionStorage, SessionStorageError>;
}
```

```rust
pub(crate) enum SessionStorageErrorKind {
    UnsupportedFormatVersion,
    HeaderCorrupt,
    HistoryTooLarge,
    StorageUnavailable,
    InvariantViolation,
}

pub(crate) struct SessionStorageError {
    pub kind: SessionStorageErrorKind,
    // private source; public projection is typed/redacted
}

pub(crate) struct StagedSessionStorage {
    // unpublished target and validated initial SessionHeader
}

pub(crate) struct OpenedSession {
    pub replay: ReplayedSession,
    pub recorder: Arc<SessionRecorder>,
}
```

Header strict failure映射HeaderCorrupt或UnsupportedFormatVersion；1 GiB/1,000,000-entry hard cap映射HistoryTooLarge；ordinary recorder initialization/lease failure仍可以创建Degraded loaded Session。Runtime public mapping分别使用DurableStateCorrupt、DurableStateTooLarge或Unavailable，不能解析private source string。

`StagedSessionStorage`只能由Agent/Session lifecycle publication路径消费，不能被query、Load或Turn admission观察。

`stage_create()`写入initial SessionHeader但不创建SessionRecorder；Agent/Session lifecycle只在该staging与SessionDefinition都成功后原子发布`Open + Unloaded`Session。header staging失败使Create失败，partial staging target不进入catalog。

SessionHeader中的initial Agent/definition refs只承担file identity和creation provenance。Header没有EntryId，不是Session definition/lifecycle change log、current authorization proof或old execution recovery source；Load仍从Agent/Session durable owner读取current head。

`open()`始终尝试初始化Recorder；lease或初始化失败返回`RecordingHealth::Degraded`的loaded Session。history可读且Workspace/definition可用时仍允许Turn admission。Recorder初始化和header append都不创建后台worker。

不提供：

```text
SessionWriter::append
append_raw_json
caller-provided projection delta
repair middle line
recording failure rollback
ResumeSessionRecording / StartRecordingSegment
unrecorded suffix backfill
physical commit receipt as execution permit
```

## 物理文件

物理encoding、field order、limits和bounded scanner的exact owner是[Conversation JSONL Format V1](../formats/conversation-jsonl-v1.md)。semantic overview：

```text
sessions/<SessionId>.jsonl
line 1   session_header
line 2+  entry
```

```rust
pub struct SessionHeader {
    pub format_version: u32,
    pub session_id: SessionId,
    pub created_at: Timestamp,
    pub initial_agent: AgentRevisionRef,
    pub initial_definition_revision: SessionDefinitionRevision,
}
```

```rust
pub struct StoredSessionEntry {
    pub entry_id: EntryId,
    pub parent_id: Option<EntryId>,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub timestamp: Timestamp,
    pub body: StoredEntryBody,
}
```

```rust
pub enum StoredEntryBody {
    UserMessage(StoredUserMessage),
    AssistantMessage(StoredAssistantMessage),
    ToolMessage(StoredToolMessage),
    InteractionRequested(StoredInteractionRequest),
    InteractionResolved(StoredInteractionResolution),
    Compaction(StoredCompaction),
}
```

每行immutable。EntryId由live owner在apply前分配并进入collision guard，Recorder不能创建或改写。v1只记录Turn-scoped conversation/Interaction/Compaction facts，因此turn_id required；旧optional形状只服务已删除的Session configuration/lifecycle entries，不进入format v1。

## Stored Messages

```rust
pub struct StoredUserMessage {
    pub item_id: ItemId,
    pub source: UserMessageSource,
    pub content: CanonicalUserMessage,
}
```

Input与Steer通过`StoredSessionEntry.turn_id`和`UserMessageSource`区分。TurnId只用于history grouping和Item/Tool correlation；replay不从它重建TurnStatus。实际响应模型保存在对应`StoredAssistantMessage.model`，future Turn配置从current durable Session/Agent definition重新capture。

`CanonicalUserMessage`是`MessageRecord + Arc<[PromptContributionStamp]>`的唯一事实源。Storage不定义`StoredPromptContributionStamp`，也不在StoredUserMessage上保存第二份stamp。live reducer、JSONL encoder和Prompt assembly使用同一个safe part-level stamp表示。该storage projection消费[INV-202](../architecture.md#跨模块不变量索引)。

Session JSONL不保存Turn-static Prompt baseline、`PromptContent`、source locator、cache key或`PromptContentRef`。Prompt/Skill/Workspace正文只有在已经规范化为CanonicalUserMessage内容时才成为conversation fact。stamp只保存`content_part_index`以及`SkillId`或`WorkspaceRootKey + WorkspaceRelativePath`；不保存name、绝对路径、canonical root、trust revision、authorization、hash、cache key或正文引用。

```rust
pub struct StoredAssistantMessage {
    pub disposition: AssistantDisposition,
    pub content: Arc<[StoredAssistantContent]>,
    pub model: ModelResponseSummary,
    pub response_id: Option<ProviderResponseId>,
    pub finish_reason: ModelFinishReason,
    pub effective_max_output_tokens: NonZeroU32,
    pub usage: Option<ModelUsage>,
    pub logical_retry_count: u8,
    pub metadata: ProviderResponseMetadata,
}
```

```rust
pub enum StoredAssistantContent {
    Reasoning {
        item_id: ItemId,
        content: ReasoningContent,
    },
    Text {
        item_id: ItemId,
        text: Arc<str>,
    },
    ToolCall {
        item_id: ItemId,
        tool_call_id: ToolCallId,
        name: ToolName,
        arguments: BoundedJsonObject,
    },
}
```

```rust
pub struct StoredToolMessage {
    pub item_id: ItemId,
    pub tool_call_id: ToolCallId,
    pub outcome: StoredToolOutcome,
}

pub enum StoredToolOutcome {
    Completed {
        source: ToolOutcomeSource,
        disposition: ToolResultDisposition,
        content: ToolResultContent,
    },
    Abandoned {
        reason: ToolAbandonReason,
    },
}
```

每个StoredAssistantContent直接携带其ItemId；禁止`item_ids[] + content[]`平行数组。entry内ItemId unique，ToolCallId在response内unique，content保持provider finalized semantic order。response ID、finish reason、effective max output、usage、retry和metadata与ModelResponseSummary分开保存；exact validation与wire见[Format V1 · Assistant Message](../formats/conversation-jsonl-v1.md#assistant-message)。

Conversation Storage直接复用Wire-owned BoundedJsonObject、ModelGateway-owned ReasoningContent/ModelResponseSummary/usage/finish/metadata，以及Tools-owned ToolOutcomeSource/ToolResultContent/ToolResultDisposition/ToolAbandonReason；不定义StoredReasoning、unbounded JsonValue或第二套Tool disposition。ToolResult.details是process-local/private debug value，format v1不记录。

Tool durable correlation继续使用`TurnId + ItemId + ToolCallId`。ToolCallId只要求同一assistant response内唯一。

## Stored Interactions

```rust
pub struct StoredInteractionRequest {
    pub request_id: RequestId,
    pub item_id: ItemId,
    pub request: StoredInteractionRequestBody,
}

pub enum StoredInteractionRequestBody {
    ToolApproval(ToolApprovalRequestView),
    UserQuestion(UserQuestionRequest),
}

pub struct StoredInteractionResolution {
    pub request_id: RequestId,
    pub item_id: ItemId,
    pub resolution: StoredInteractionResolutionBody,
    pub resolution_key: Option<InteractionResolutionKey>,
}

pub enum StoredInteractionResolutionBody {
    ToolApproval(ToolApprovalResolution),
    UserAnswer(UserQuestionAnswer),
    Cancelled(InteractionCancelReason),
}
```

Stored interactions只保留loaded execution中具有合法single producer、EntryId owner和historical conversation意义的facts。request/resolution使用Runtime public view同源的bounded safe types：Tool approval只保存redacted summary、safe options与selected index/kind，不保存private option→PermissionSet map；UserQuestion只保存non-secret Text/SingleChoice request和answer；Cancelled保存closed InteractionCancelReason。任意host Resolve的resolution_key为Some，owner-driven closure为None。exact relation/wire见Format V1的[request](../formats/conversation-jsonl-v1.md#interaction-requested)与[resolution](../formats/conversation-jsonl-v1.md#interaction-resolved)。format v1没有secret/password/credential variant，raw secret不得进入JSONL。

Agent/Session definition、metadata、Open/Archived/Deleted transition和load/readiness变化不写JSONL：

```text
Agent/Session durable owner updates head/revision/lifecycle
→ update Runtime query/snapshot/typed event observer surface
→ no LiveSessionState EntryId allocation
→ no SessionRecorder call
```

Runtime observer output不是durable transition log。restart从entity durable head恢复current Agent/Session state，从JSONL恢复conversation；两者按owner组合，不从conversation反推configuration timeline。metadata使用Runtime Interface冻结的独立event kind，不影响本节的no-JSONL owner规则。

JSONL不保存`TurnStarted`、`TurnCompleted`、`TurnInterrupted`、`TurnFailed`或`StoredTurnTerminal`。Final Assistant是稳定conversation fact；Interrupted/Failed只属于当前loaded execution的StateEvent/Snapshot。

不记录：

- `ToolExecutionStarted`；
- `ToolRoundCompleted`；
- `RunningOperation`；
- ActiveTurn phase；
- TurnStatus和Turn terminal reason；
- provider stream delta；
- retry timer；
- recorder health changes；
- process-local cancellation epoch。

## StoredCompaction

```rust
pub struct StoredCompaction {
    pub summary: String,
    pub first_kept_entry_id: Option<EntryId>,
    pub model_call: Option<StoredCompactionModelCall>,
}
```

`StoredCompactionModelCall`的唯一semantic定义位于[Compaction](compaction.md#summary-validation与provenance)。automatic SummaryModel路径始终为`Some`；Conversation Storage只负责[format-v1 projection](../formats/conversation-jsonl-v1.md#compaction)，不复制第二份provenance semantic type。summary<=65,536 bytes，finish/retry/request max/metadata使用Format v1 exact limits。

recorded marker只影响future replay。live Replace在inline record attempt前已经生效。marker缺失时restart恢复旧conversation；marker存在但无法在当前effective stable-unit projection上匹配index大于0的unit `first_entry_id`时，replay忽略并报告diagnostic。`None`只在当前effective conversation非空时表示覆盖全部units。

## History Tree

`EntryId + parent_id`形成recorded tree。ordinary live mutation的parent使用当前live selected head。由于Recorder只写连续前缀，正常failure不会主动写出缺失parent后的suffix。

cold file仍可能因手工编辑、旧bug或partial migration包含orphan；replay隔离而不是brick全部history。

不建立Branch entity。selected path由root到chosen leaf确定。

## Tolerant Replay

**Canonical cross-module invariant: INV-002.**

replay按[Format V1 bounded decode](../formats/conversation-jsonl-v1.md#bounded-decode与replay)顺序：

1. 检查whole-file physical byte cap；超过1 GiB返回HistoryTooLarge，不truncate tail；
2. strict解析Header；oversized/malformed/invalid UTF-8/unsupported version立即fail Load；
3. 以bounded scanner读取newline-terminated entries并在scan中执行1,000,000 complete-entry cap；oversized line stream-discard到LF，invalid UTF-8/malformed/unknown variant skip + diagnostic；
4. UserMessage正文与解释性contribution stamps独立校验；未知origin、malformed stamp或越界`content_part_index`直接丢弃；同一part first valid stamp wins；合法正文保留；
5. duplicate EntryId使用first valid wins并告警；
6. missing parent形成isolated orphan root；
7. invalid Session/Turn/Item/Tool/Interaction relation只隔离对应projection；
8. invalid Compaction marker忽略该effect并diagnose；
9. whole-file cap已通过时，最后unterminated bytes作为partial tail忽略；
10. 构建recorded tree、history view和sanitized conversation；
11. 保留最多100条redacted diagnostic detail，额外按code aggregate并追加truncated summary。

Replay不恢复任何process-local执行对象，也不根据stamp重新加载Skill/Workspace正文、重新授权source或重建旧PromptSet。conversation正文是恢复正确性的事实，stamp只承担安全解释作用。

## Tool Exchange Replay

cold projector维护每个assistant response的expected ordered ToolCalls：

- 每个call最多接收一个first valid terminal outcome；
- duplicate ToolResult first valid wins；
- ToolResult与ToolAbandoned冲突first terminal wins；
- 全部first terminal outcome均为matching truthful ToolResult时，exchange model-visible；
- abandoned-first、missing、orphan或identity-conflicting exchange不进入模型conversation；
- 下一条合法User、Assistant或Compaction关闭未完成exchange；EOF处仍未完成的exchange直接排除；
- closure后迟到result视为orphan；
- 后续合法conversation可以继续恢复。

该projector只服务cold replay。live complete-exchange owner见[Turn / Item / Interaction](turn-item-interaction.md#complete-tool-exchange)。

## Live与Cold Conversation

```rust
pub(crate) struct LiveConversationView {
    revision: ConversationRevision,
    messages: Arc<[ModelMessage]>,
}

pub(crate) struct ReplayedConversationView {
    recorded_head: Option<EntryId>,
    messages: Arc<[ModelMessage]>,
    diagnostics: Arc<[SessionReplayDiagnostic]>,
}
```

PromptSet可消费两者共同实现的private sanitized view interface。它们都保证provider-valid Tool exchange，但只有Live view参与当前Turn execution。

Compaction通过同一个live reducer取得额外的crate-private projection：

```rust
impl LiveSessionState {
    pub(crate) fn compaction_source_view(
        &self,
    ) -> Arc<LiveCompactionSourceView>;
}
```

`LiveCompactionSourceView`及stable-unit shape由[Compaction](compaction.md#stable-unit-source)唯一拥有。reducer负责把ordinary User、无ToolCall Assistant、完整Assistant+ToolResults exchange和leading rolling summary分组成exact EntryId-bearing units；Compaction负责估算和cut。每个rolling summary unit的origin是安装它的外层StoredCompaction entry ID，Tool exchange只能以Assistant entry作为marker boundary。该projection与ordinary `LiveConversationView`在同一次短guard内从同一revision构造，guard在planning或任何await前释放。

不再存在`CommittedConversationView`或storage签发的推进permit。

## Fork

**Canonical cross-module invariant: INV-004.**

Fork使用[Runtime Interface](runtime-interface.md#forkanchor)定义的公开`ForkSourceKind::LiveSnapshot | RecordedHistory`；本节是source selection与copy语义的canonical owner。

Fork source由source Session在residency/lifecycle synchronization内的状态决定：

```text
loaded at source linearization point
→ ForkSourceKind::LiveSnapshot

unloaded at source linearization point
→ ForkSourceKind::RecordedHistory
```

loaded source始终使用LiveSnapshot，不根据Recorder是Healthy、Degraded或某条entry是否已经写入而切换到RecordedHistory。Unload先完成时按unloaded路径读取RecordedHistory；Fork先完成snapshot capture时保持LiveSnapshot，后续Unload不改变已捕获source。

LiveSnapshot在一次短live-state critical section内完成：

```text
resolve public ForkAnchor
→ capture exact selected path
→ clone immutable recordable facts and stable IDs
```

anchor解析和selected path必须来自同一个snapshot。capture前已经apply的live mutation会被复制，即使对应`SessionRecorder.record().await`仍在执行；capture后才apply的mutation不会被复制。stream draft、ProgressEvent、pending queue value和其他未apply状态不属于snapshot。source guard必须在target staging I/O前释放。

RecordedHistory从tolerant replay得到的selected recorded path复制。两种source都保留复制历史的Entry/Turn/Item/Request/ToolCall IDs，只分配新SessionId；future entry使用fresh ID。

target使用staging + atomic publication建立完整新record stream。selected path未全部materialize或replay validation失败时Fork返回typed error且不发布child；这一步是Fork publication前的完整copy，不是已发布Session上的best-effort suffix recording。child后续和其他Session一样始终尝试记录。selected path可以在任意公开message anchor结束；child不继承source active Turn状态，发布后保持Unloaded，未来Load得到Idle conversation view。

child durable provenance至少保存：

```rust
pub struct SessionForkProvenance {
    pub source_session_id: SessionId,
    pub source: ForkSourceKind,
    pub anchor: ForkAnchor,
}
```

Fork不复制ActiveTurnTask、Tool process、Interaction waiter、Steer/FollowUp queue、CancellationToken、Recorder object或in-flight append。

Fork也不复制source Session definition/metadata/lifecycle timeline。child current definition由Agent/Session lifecycle staging从source durable definition构造child-local revision；selected path只包含conversation、Interaction和Compaction facts。

## Recovery

```text
open file and attempt writable lease
→ tolerant replay recorded prefix
→ writable open truncates only final unterminated partial tail ignored by replay
→ create LiveSessionState from replay
→ sanitize incomplete exchange
→ initialize new inline recorder at replayed recorded head
→ read current Agent/Session durable heads and resolve current Workspace definition
→ current_turn = None
→ SessionExecutor Idle or WorkspaceUnavailable
```

recording unavailable不阻止admission。新loaded instance始终尝试初始化Recorder，并根据open结果初始化为Healthy或Degraded；它不继承旧loaded instance的health object。Unload/Load永久丢弃旧unrecorded live tail和旧TurnStatus。Load不推断旧Turn outcome、不创建restart interruption，也不执行recovery append。

Tail处理只允许在valid v1 Header、exclusive writable lease且whole-file cap通过后，截断final unterminated partial line到last LF。完整newline-terminated entry即使oversized、malformed、unknown或来自先前outcome-unknown write，也必须保留physical bytes；typed replay决定接受或隔离，不执行middle repair、reparent或gap marker synthesis。

## Diagnostics

```rust
pub struct SessionReplayDiagnostic {
    pub code: SessionReplayDiagnosticCode,
    pub entry_id: Option<EntryId>,
    pub line_number: Option<u64>,
    pub redacted_detail: Option<String>,
}
```

Diagnostics exact code、100-detail cap、512-byte redacted message和aggregate规则由[Wire Schema](wire-schema.md#diagnostics)与[Format V1](../formats/conversation-jsonl-v1.md#bounded-decode与replay)拥有。禁止暴露raw line、credential、绝对敏感路径、完整Tool output、OS error或provider payload。

## 测试要求

- Create的SessionHeader staging失败或publication前crash不产生catalog-visible Session；
- Create只写SessionHeader；definition/metadata/lifecycle mutation不生成StoredSessionEntry；
- Load始终尝试初始化Recorder，失败得到Degraded而不是Disabled；
- live owner在apply前分配EntryId，Recorder观察到exact same ID；
- replay与Fork copied IDs正确seed collision guard，future ID不碰撞；
- Degraded状态继续分配ID且不复用未publish ID；
- recorder按live mutation顺序inline写出；
- encode/write failure后停止suffix，replay保持有效完整行前缀；
- recording failure不阻止Model/Tool/Interaction或live terminal settlement/publication；
- crash或failed write留下partial tail时read-only replay安全忽略，writable Load只截断final unterminated tail；
- Degraded后即使storage恢复，当前loaded Session仍不写新entry；
- Unload/Load只恢复recorded prefix并可建立新的Healthy Recorder；
- 不创建segment、gap marker或backfill suffix；
- malformed/duplicate/orphan/partial-tail replay；
- strict Header version/UTF-8/64KiB，entry 1MiB，file 1GiB/1,000,000 lines exact boundary/+1；
- oversized complete line后续valid line继续恢复，完整bad line不truncate；
- byte-exact golden覆盖六种StoredEntryBody、all option null和canonical field order；
- malformed、unknown或越界contribution stamp被丢弃；同一part重复stamp first valid wins；合法UserMessage正文继续恢复；
- complete/incomplete/duplicate/conflicting Tool exchange；
- Assistant content ItemId内联且unique，不存在parallel item_ids/content错位；
- Assistant response/model/finish/max-output/usage/retry/metadata round-trip；
- Stored Tool Completed/Abandoned closed variants且不保存details；
- ToolApproval/UserQuestion request/resolution/cancel reason/key relation；
- compaction marker missing/invalid/指向第一个unit或ToolResult内部时忽略；
- duplicate identical messages仍按stable-unit EntryId定位exact marker；
- repeated rolling summary使用前一StoredCompaction outer EntryId作为source origin；
- fork历史ID保留和future ID无collision；
- loaded Fork包含snapshot capture前已apply的unrecorded tail，unloaded Fork只复制RecordedHistory；
- live mutation apply后、record attempt返回前Fork仍使用LiveSnapshot包含该mutation；
- Fork与Unload竞态产生稳定且公开的LiveSnapshot或RecordedHistory source；
- live Fork staging失败不发布partial child；
- loaded/unloaded Fork不复制source definition或lifecycle timeline；child definition来自独立lifecycle staging；
- Fork不追加fork-specific terminal，child不继承source current Turn；
- restart不恢复execution objects；
- restart后current_turn为空且不合成Turn terminal；
- bounded redacted diagnostics；
- LiveConversation与无corruption replay产生相同sanitized messages。

## 后续扩展

format v1 wire、IDs/revisions、line/file limits、Stored DTO、Compaction projection、diagnostic cap和tolerant scanner已由ADR 0134与[exact format](../formats/conversation-jsonl-v1.md)冻结。future工作只包括显式format-v2 migration/repair utility、blob/artifact side channel和超过v1 hard caps的export方案；这些能力不能修改v1 reader/writer语义。Compaction stable-unit source由ADR 0132关闭，EntryId owner由Q9关闭，Turn lifecycle omission与无closure recovery由Q10/ADR 0127关闭，configuration/lifecycle owner由ADR 0131关闭，Prompt stamp由ADR 0129关闭。
