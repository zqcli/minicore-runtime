# Conversation Recording 与 Replay 架构设计

状态：当前权威架构（ADR 0134，受ADR 0136/0137 durable/async refinements约束；M3.1/M3.2、M5 recovery/root-lease foundation、crate-private Agent Create、ordinary Session Create canonical Header/same-file proof publication及unloaded RecordedHistory + Genesis Fork source-Header tracer完成；published conversation target/writable proof、non-Genesis/LiveSnapshot Fork semantic streaming、Recorder与semantic replay pending）
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
- Recorder不使用persistent worker、channel或process-local background queue；每次append只使用一个owner-tracked blocking job；
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
   ├─ ordered best-effort append only
   ├─ JSONL encoder
   └─ RecordingHealth

ConversationStorage
├─ Header/entry rewrite and validation
├─ tolerant replay and recorded tree/query
└─ LiveForkConversationSeed / RecordedForkConversationLease semantic seed

DurableState
└─ physical target/Fork sink, root lease, final publication and cleanup

Lifecycle and Runtime orchestrate these owners; Recorder does not own replay/tree/fork publication, and ConversationStorage never owns an entity path or publication marker.
```

Live reducer拥有domain validation、EntryId allocation和当前进程conversation。Recorder只接受private constructors创建的immutable `StoredSessionEntry`，不重新决定Model/Tool协议或identity。

## EntryId Ownership

```rust
pub(crate) struct EntryIdGenerator {
    // private CSPRNG + collision guard
}

impl EntryIdGenerator {
    pub(crate) fn allocate(&mut self) -> Result<EntryId, EntryIdAllocationError>;
}

pub(crate) enum EntryIdAllocationError {
    EntropyUnavailable,
    CollisionAttemptsExhausted,
}
```

`EntryIdGenerator`由`LiveSessionState`私有持有，不暴露给SessionRecorder、Tool、ModelGateway、Prompt或Runtime adapter。`EntryIdAllocationError`是owner-local typed/redacted error：不得携带raw entropy/OS source，`Debug`与public diagnostic不得泄漏其内部原因。它不是panic/fatal assumption，owner把失败作为普通typed mutation failure处理。owner规则：

- domain validation成功后、live apply之前分配EntryId；
- ordinary mutation的`parent_id`同时绑定当前live selected head；
- `allocate()`每次从16 CSPRNG bytes形成candidate，最多尝试32次；只有unique candidate才在return前立即reserve到collision guard，同一loaded instance内不复用；
- entropy失败返回`EntropyUnavailable`，32次均碰撞返回`CollisionAttemptsExhausted`；两种失败都不改变live state、selected head或ConversationRevision；
- replay初始化时用文件中全部first-valid EntryId seed guard，不只使用selected model-visible path；
- loaded Fork child用已经materialize到target的全部copied EntryId seed guard，future entry分配fresh ID；
- Degraded时generator继续工作，recording failure不改变已经进入live Item/Interaction/Compaction的identity；
- EntryId不从JSONL line number、storage ordinal、ConversationRevision或time派生。

EntryId exact wire由[Wire Schema typed carrier](wire-schema.md#runtime-generated-ids)冻结为`ent_<32 lowercase hex>`；generator使用16 CSPRNG bytes且不承载时间/顺序。CSPRNG failure或collision exhaustion不能退化为revision/ordinal/time-derived ID，也不能panic。Recorder只验证entry identity已经存在且relation合法，不能创建、替换或规范化EntryId。

## Live Mutation And Recording

**Canonical cross-module invariant: INV-001.**

```text
validate relation/value and project Prompt/body value
→ prepare complete infallible state delta + StoredSessionEntry body
→ determine ConversationRevision delta; when +1, ConversationRevision.checked_next()
→ LiveSessionState.EntryIdGenerator.allocate()? + bind current parent_id
→ infallibly construct exact Arc<StoredSessionEntry>
→ infallibly bind any new-origin PreparedLiveCompactionUnit
→ commit prepared state delta
→ append that same Arc to the full selected path
→ install preflighted ConversationRevision when delta is +1
→ await SessionRecorder.record(the same Arc)
→ publish final StateEvent / resume waiter / continue protocol
```

All ordinary `Result`-returning validation, projection and candidate preparation finishes before allocation. This includes `PreparedLiveCompactionUnit` construction; a new User, ordinary Assistant or rolling summary binds its prepared unit only after its new ID exists, while a complete exchange may bind with its already-existing Assistant origin before the current Tool entry allocation. checked revision overflow必须在任何EntryId allocation或state mutation前失败。EntryId allocation失败同样不改变state/head/revision；only a successfully returned ID is reserved. After allocation the normal path cannot return an error: exact entry-Arc construction, new-origin prepared-unit binding, state commit, same-Arc path append and preflighted revision installation are infallible, while ordinary process allocation panic is outside the Result contract. **State is never applied before its exact entry Arc is constructed.** Therefore a returned error never consumes an ID. `record().await`返回：

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
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct ConversationRevision(u64);

pub(crate) struct LiveConversationError {
    reason: LiveConversationErrorReason,
    // private redacted state context
}

pub(crate) enum LiveConversationErrorReason {
    RevisionOverflow,
    EntryIdAllocation,
    InvalidRelation,
    InvalidTurn,
    InvalidPromptProjection,
    InvalidCompactionSource,
    StaleCompactionSource,
    InvalidCompactionCut,
    CompactionMarkerMismatch,
    PendingToolExchange,
    InteractionConflict,
}

impl ConversationRevision {
    pub(crate) fn checked_next(self) -> Result<Self, LiveConversationError>;
}
```

`LiveConversationError` is owner-local typed/redacted error; fields/source and `Debug` must not disclose Prompt body, Tool result, reasoning, provider payload or entropy detail. `InvalidRelation` covers an invalid canonical body/relation combination; `InvalidTurn` covers current/start Turn semantics; `InvalidPromptProjection` privately maps any transcript `ModelMessageError` needed to project an otherwise canonical fact. Public `PromptValueError` is not extended or wrapped for M4 transcript construction. `InvalidCompactionCut` distinguishes an out-of-range nonzero cut from a valid cut with a wrong marker; zero is rejected by the `NonZeroUsize` API boundary before reducer apply. `LiveSessionState` privately maps each Compaction-owned `CompactionSourceError` from source construction to its own `InvalidCompactionSource` reason; it neither returns/wraps that foreign error nor asks Compaction to depend on this type. `ConversationRevision`是process-local、单调的**live-conversation operation basis**，不是当前可见`ModelMessage`集合的hash、版本或计数。loaded Session从0开始，ModelCallRequest、logical retry和Compaction source/plan捕获exact值；它不写入JSONL，也不跨restart比较。`checked_next()`是唯一normal-result overflow seam：它在所有`+1` mutation的EntryId allocation前运行，overflow返回`LiveConversationError`，不能wrap、saturate、mutate state或消耗identity。

精确delta矩阵：

| 成功live apply | revision delta |
| --- | ---: |
| accepted Input UserMessage或Steer UserMessage | +1 |
| every accepted Assistant，包括含ToolCalls但尚未model-visible的Assistant | +1 |
| 在每个expected call都得到first truthful `Completed` result时，complete Tool exchange promotion | +1 |
| partial Tool terminal、abandoned或其他non-visible exchange settlement | +0 |
| Interaction request/resolution | +0 |
| progress、usage-only或recording-health change | +0 |
| Compaction Replace | +1 |
| failed apply或idempotent apply | +0 |

因此含ToolCalls的Assistant在模型仍看不见它时已经改变operation basis；稍后全部matching `Completed` result把完整exchange提升为model-visible时又改变一次。该双增量防止in-flight Model/Compaction/retry把hidden assistant或刚promotion的exchange误当成同一操作基础。所有`+1`先执行checked increment；overflow在EntryId allocation与任何state mutation前失败，不能wrap、saturate或消耗identity。

EntryId继续存在，但只承担history identity和引用。ConversationRevision承担process-local live-conversation operation basis；两者不能互相派生。

## Live Reducer Transaction 与Read Interface

`LiveSessionState`是recordable conversation mutation的唯一transaction owner。它私有拥有loaded `SessionId`、完整immutable selected path、`EntryIdGenerator`、`ConversationRevision`、Item relation、Pending Interaction和reducer-maintained stable units；EntryId-only path不足以materialize unrecorded live tail，因此不能服务future loaded `LiveSnapshot`/Fork。caller不能提交或替换其中任一envelope/identity value：

```rust
pub(crate) struct LiveSessionState {
    session_id: SessionId,
    selected_path: Vec<Arc<StoredSessionEntry>>,
    entry_ids: EntryIdGenerator,
    revision: ConversationRevision,
    relations: Vec<ItemRelation>,
    interactions: Vec<Interaction>,
    stable_units: Vec<LiveCompactionUnit>,
    // private live conversation, expected-exchange and Item state
}

pub(crate) struct AppliedConversationFact {
    entry: Arc<StoredSessionEntry>,
    revision: ConversationRevision,
}

impl AppliedConversationFact {
    pub(crate) fn entry(&self) -> &Arc<StoredSessionEntry>;
    pub(crate) fn revision(&self) -> ConversationRevision;
}

impl LiveSessionState {
    fn selected_head(&self) -> Option<&EntryId> {
        self.selected_path.last().map(|entry| &entry.entry_id)
    }
}

pub(crate) enum InteractionResolutionApplyOutcome {
    Applied(AppliedConversationFact),
    Idempotent { revision: ConversationRevision },
}

impl LiveSessionState {
    pub(crate) fn apply_user_message(
        &mut self,
        body: StoredUserMessage,
        turn_id: TurnId,
        timestamp: Timestamp,
    ) -> Result<AppliedConversationFact, LiveConversationError>;

    pub(crate) fn apply_assistant_message(
        &mut self,
        body: StoredAssistantMessage,
        turn_id: TurnId,
        timestamp: Timestamp,
    ) -> Result<AppliedConversationFact, LiveConversationError>;

    pub(crate) fn apply_tool_message(
        &mut self,
        body: StoredToolMessage,
        turn_id: TurnId,
        timestamp: Timestamp,
    ) -> Result<AppliedConversationFact, LiveConversationError>;

    pub(crate) fn apply_interaction_request(
        &mut self,
        candidate: InteractionRequestCandidate,
        turn_id: TurnId,
        timestamp: Timestamp,
    ) -> Result<AppliedConversationFact, LiveConversationError>;

    pub(crate) fn apply_interaction_resolution(
        &mut self,
        candidate: InteractionResolutionCandidate,
        timestamp: Timestamp,
    ) -> Result<InteractionResolutionApplyOutcome, LiveConversationError>;

    pub(crate) fn apply_compaction(
        &mut self,
        source: Arc<LiveCompactionSourceView>,
        cut: NonZeroUsize,
        replacement: CompactionReplacement,
        turn_id: TurnId,
        timestamp: Timestamp,
    ) -> Result<AppliedConversationFact, LiveConversationError>;

    pub(crate) fn capture_conversation_views(
        &self,
    ) -> Result<CapturedConversationViews, LiveConversationError>;
}
```

不定义`UserMessageCandidate`、`AssistantMessageCandidate`或`ToolMessageCandidate`，也没有generic candidate abstraction。ordinary apply只消费existing valid-by-construction `StoredUserMessage`、`StoredAssistantMessage`或`StoredToolMessage` body；raw replay reconstruction不是这些live apply的入口。reducer在allocation前仅经Prompt-owned constructors从这些canonical body facts完成所需provider-neutral projection。任何apply都不接受prebuilt `StoredSessionEntry`或caller-selected entry envelope identity：`LiveSessionState`绑定`SessionId`、current parent和`EntryId`；caller显式提供typed `TurnId`与`Timestamp`。reducer按current/start semantics验证supplied `TurnId`，所以尤其在Input start之前不得声称它能从state导出TurnId或timestamp。`Timestamp`是owning Session/Turn orchestration提供的typed fact，reducer不读取ambient clock。

Interaction是唯一必要的private-candidate exception，因为safe durable projection必须与private live request/resolution共存：

```rust
pub(crate) struct InteractionRequestCandidate {
    request_id: RequestId,
    item_id: ItemId,
    request: InteractionRequest,
}

impl InteractionRequestCandidate {
    pub(crate) fn new(
        request_id: RequestId,
        item_id: ItemId,
        request: InteractionRequest,
    ) -> Self;
}

pub(crate) struct InteractionResolutionCandidate {
    request_id: RequestId,
    resolution_key: Option<InteractionResolutionKey>,
    resolution: ResolvedInteraction,
}

pub(crate) struct InteractionCandidateError {
    reason: InteractionCandidateErrorReason,
}

pub(crate) enum InteractionCandidateErrorReason {
    InvalidResolutionOrigin,
}

impl InteractionResolutionCandidate {
    pub(crate) fn host(
        request_id: RequestId,
        resolution_key: InteractionResolutionKey,
        resolution: ResolvedInteraction,
    ) -> Result<Self, InteractionCandidateError>;

    pub(crate) fn owner_cancellation(
        request_id: RequestId,
        resolution: ResolvedInteraction,
    ) -> Result<Self, InteractionCandidateError>;
}
```

Both candidate fields are private and their `Debug` redacts the request, resolved value and host key. `InteractionCandidateError` and its private reason are crate-private/redacted; `Debug`、`Display` and its source chain disclose neither resolution payload nor key material. `InteractionRequestCandidate::new()` accepts only the owner `InteractionRequest`, never a caller-built `StoredInteractionRequest`/safe view. `host()` is fallible: it seals `Some(key)` only for ToolApproval, UserAnswer or `Cancelled(HostCancelled)`. `owner_cancellation()` is fallible: it seals `None` only for `Cancelled` with a non-Host reason. Each constructor checks the opaque `ResolvedInteraction`'s private live resolution and rejects a wrong origin as `InvalidResolutionOrigin` **before `LiveSessionState::apply_interaction_resolution()` and therefore before any EntryId allocation**. Request apply receives the candidate plus caller-supplied `TurnId` and `Timestamp`. Resolution apply receives only the candidate plus `Timestamp`: it loads the exact stored pending request, derives its TurnId, ItemId and request/resolution family, validates the key/family matrix, derives the safe `StoredInteractionResolution` body itself, and retains the candidate's owner `ResolvedInteraction` for live waiter routing. Thus a resolution caller never resupplies TurnId or ItemId. It accepts neither a caller-built stored interaction body nor any entry envelope identity; timestamp remains a typed orchestration fact rather than ambient clock access.

Every normal-result fallible step—relation/value validation, Prompt projection, interaction/source checks, `ConversationRevision::checked_next()`, `PreparedLiveCompactionUnit` construction and prepared state-delta construction—finishes **before** `EntryIdGenerator::allocate()`. After allocation, it first constructs the exact `Arc<StoredSessionEntry>`, then infallibly binds any new-origin prepared stable unit, commits the already-prepared in-memory delta, appends that same Arc to `selected_path`, and installs the preflighted revision. For Compaction Replace, the reducer has already consumed `CompactionReplacement::into_parts()`; it may clone that prebuilt immutable rolling-summary message into the leading unit and flattened `LiveConversationView`, and clones retained unit handles only from the fresh current source. It never reconstructs either from borrowed messages or a caller suffix. None of those steps returns an error; ordinary process allocation panic is outside this `Result` contract. State is never committed before the entry Arc exists. Therefore a returned error never consumes an ID. Every successful recordable apply returns and appends the **exact same** `Arc<StoredSessionEntry>` in `AppliedConversationFact`; a Recorder receives that same Arc, never a rebuilt or merely equal entry.

Interaction same-key/same-canonical-payload resolution returns `InteractionResolutionApplyOutcome::Idempotent` without allocation, entry, record attempt or event. Same-key/different-payload and terminal/different-key failures return typed errors; only the first successful resolution returns `Applied`.

```rust
pub(crate) struct PendingInteractionFact {
    request_id: RequestId,
    turn_id: TurnId,
    item_id: ItemId,
    request: InteractionRequestView,
}

impl PendingInteractionFact {
    pub(crate) fn request_id(&self) -> &RequestId;
    pub(crate) fn turn_id(&self) -> &TurnId;
    pub(crate) fn item_id(&self) -> &ItemId;
    pub(crate) fn request(&self) -> &InteractionRequestView;
}

pub(crate) struct CapturedConversationViews {
    conversation: LiveConversationView,
    compaction_source: Arc<LiveCompactionSourceView>,
    selected_head: Option<EntryId>,
    relations: Arc<[ItemRelation]>,
    pending_interactions: Arc<[PendingInteractionFact]>,
}

impl CapturedConversationViews {
    pub(crate) fn conversation(&self) -> &LiveConversationView;
    pub(crate) fn compaction_source(&self) -> &Arc<LiveCompactionSourceView>;
    pub(crate) fn selected_head(&self) -> Option<&EntryId>;
    pub(crate) fn relations(&self) -> &[ItemRelation];
    pub(crate) fn pending_interactions(&self) -> &[PendingInteractionFact];
}
```

`capture_conversation_views()` performs one short immutable capture from the same state and exact revision, maps any Compaction-owned source-factory error to `LiveConversationError`, then returns this one crate-private aggregate. `conversation`, Compaction source, selected head, relations and safe Pending Interaction request views cannot be mixed from separate captures. `CapturedConversationViews` deliberately exposes only the head derived from `selected_path.last()` because M4 read scope is narrow; `LiveSessionState` still retains the full `Vec<Arc<StoredSessionEntry>>` applied facts for a later loaded `LiveSnapshot`/Fork capture. It is neither M8's public `SessionSnapshot`/Item/Interaction DTO nor Fork's `LiveSnapshot`; M4 exposes no public read DTO. `LiveSessionState` uses Compaction-owned validated factories to form the source/unit portion, but remains the canonical producer.

## SessionRecorder

```rust
pub(crate) struct SessionRecorder {
    session_id: SessionId,
    state: std::sync::Mutex<SessionRecorderState>,
    health: ArcSwap<RecordingHealth>,
}

pub(crate) enum SessionRecorderState {
    Ready { file: std::fs::File },
    InFlight { settlement: Arc<RecorderSettlement> },
    Degraded,
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
- 每次Load都尝试初始化Recorder；ordinary conversation capability/open/permission/disk/encode/write failure while the root lease remains intact enters `Degraded`; root lease or lock-file identity loss is global poison/close, never this health state；
- 同一loaded Session只允许`Healthy → Degraded`，Recorder不会在当前load中自动恢复Healthy；
- Degraded后不自动probe/retry、不创建新segment、不保留或backfill unrecorded suffix；
- Health与当前recording diagnostic通过同一次immutable Snapshot publication对host可见。

### 顺序与Prefix规则

MVP的recordable domain mutation保持单producer ownership：Starting/Idle mutation由SessionExecutor拥有，Running Turn mutation由ActiveTurnTask拥有，Interaction routing只在对应task等待期间完成request/resolution mutation。不得让两个独立caller并发apply可记录的live mutation，再依赖Recorder mutex acquisition决定history顺序。未来出现真实multi-producer需求时必须引入显式domain sequencing，不能静默依赖scheduler顺序。

Recorder的短`std::sync::Mutex`只保护handle-transfer state，绝不保护异步append。state machine固定为：同步lock后从`Ready`移出`std::fs::File`；调用`RuntimeTaskContext::spawn_blocking_tracked`先原子预留owner-registry slot，再spawn write job；RuntimeTaskContext registry拥有实际job slot/raw `JoinHandle`，Recorder只安装其shared `RecorderSettlement`为`InFlight`，然后释放guard并在guard外await。spawn不能启动时settlement exactly-once完成，file被确定性restore或关闭并Degraded，不允许留下永远pending的`InFlight`。settlement在`Written`时同步取lock并返回/restore file到`Ready`；在write/join/panic/unknown outcome时安装terminal `Degraded`、关闭该handle并发布health。panic/unload finalizer遇到`InFlight`时clone并await同一个shared settlement；它不创建第二个job。任何raw guard都不得跨await，且没有persistent worker/background queue。`health`通过独立immutable publication读取，Snapshot不等待当前file write。Recorder按单Session顺序处理当前entry。以下任一情况首次发生后进入Degraded并停止后续append：

- encode失败；
- root lease intact时的ordinary conversation capability/open failure；
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

`RecorderWriteBarrier` is after encode/size validation and immediately before the first physical write. Before it, shutdown may skip the physical attempt only as truthful `NotRecorded` plus a Healthy→Degraded transition; it may reject the owning command with `RuntimeClosing` only if that command's corresponding live mutation has not occurred. In particular, after Submit Input live apply the original Submit still completes `TurnStarted`, then shutdown/cancel settles that same Turn. After the barrier, caller drop, Cancel, Unload, and shutdown cannot abandon the append: the shared job settles and the health transition is installed. Recorder没有background queue、flush watermark或drain operation。graceful unload waits or cancels ActiveTurnTask and then awaits the Recorder settlement; task结束后不存在待写record tail。Host可以继续unsaved execution，或显式`Unload + Load`从recorded prefix创建新的loaded instance。显式Fork在source已loaded时使用LiveSnapshot，因此可以把snapshot捕获前已apply的unrecorded tail semantic-re-encode into an independent child record stream。

## DurableState Boundary

DurableState owns the root, final entity path, initial materialization, publication and writable physical observation. Conversation Storage owns only the semantic JSONL work behind opaque capabilities issued by DurableState; it has no `SessionStorage` backend hierarchy and no caller-visible staging type.

```rust
pub(crate) struct PublishedConversationTarget {
    // private SessionId + DurableState-issued immutable file observation
}

pub(crate) struct LiveForkConversationSeed {
    // opaque captured selected entries; no long file lease after capture
}

pub(crate) struct RecordedForkConversationLease {
    // opaque selected recorded history/file observation; blocks Load/append/tail truncate
}

pub(crate) struct ExclusiveWritableConversationLease {
    // private SessionId + same-open-file proof issued by DurableState under root lease
}

pub(crate) struct ConversationStorage;

impl ConversationStorage {
    pub(crate) fn initial_header(
        seed: InitialConversationSeed,
    ) -> Result<CanonicalSessionHeader, ConversationStorageError>;

    pub(crate) async fn open(
        target: PublishedConversationTarget,
        writable: Option<ExclusiveWritableConversationLease>,
    ) -> Result<OpenedConversation, ConversationStorageError>;

    pub(crate) async fn recorded_fork_seed(
        lease: RecordedForkConversationLease,
        anchor: ForkAnchor,
    ) -> Result<RecordedForkConversationSeed, ConversationStorageError>;

    pub(crate) async fn stream_reencode_for_child(
        seed: ForkConversationSeed,
        child_session_id: SessionId,
        sink: &mut ForkConversationSink,
    ) -> Result<PreparedConversationProof, ConversationStorageError>;
}
```

`ForkConversationSeed` is closed over `LiveForkConversationSeed | RecordedForkConversationSeed`; both feed the **one semantic streaming re-encode operation**. DurableState/source-residency ownership is the sole issuer of the physical `RecordedForkConversationLease`: it binds the exact source Session/file observation and blocks Load, append and tail truncate. Conversation Storage consumes that lease to replay/resolve the anchor and create a semantic recorded seed that retains the physical lease until streaming/readback completes. `PublishedConversationTarget` and `ExclusiveWritableConversationLease` have no public or caller-visible constructor. DurableState is the sole future production issuer/factory of the latter after it binds SessionId and same-open-file observation under its held root lease; the existing `#[cfg(test)]` constructor remains M3-only until implementation. They expose neither a path nor a raw file handle to lifecycle callers. A live immutable seed needs no long file lease after capture. `ForkConversationSink` is DurableState actor-owned/tracked markerless final-path work, not a caller staging path. `PreparedConversationProof` is opaque and binds child SessionId, same file identity/length, strict Header and fully reread selected semantic path; it is checked again at PUBLISHED without allocating or rereading 1 GiB a second time.

Header strict failure maps to `HeaderCorrupt`/unsupported version and the 1 GiB/1,000,000-entry cap maps to `HistoryTooLarge`. Runtime projects those as `DurableStateCorrupt`/`DurableStateTooLarge`. Every blocking `open`/scan/replay/recorded-lease read/streamed re-encode/readback runs in an owner-tracked `RuntimeTaskContext` job; none runs directly on a current-thread Tokio worker, and caller drop cannot outlive shutdown ownership. Ordinary Recorder initialization/open/write failure is only the loaded Session's `RecordingHealth::Degraded`; it neither loses the root lease nor poisons DurableState.

DurableState creates/syncs the canonical Header before generation-1 `COMMITTED`, then creates `PUBLISHED` last. For ordinary Create, `InitialConversationSeed` is constructible only by DurableStateActor after it holds the Agent gate, reads the current Enabled exact ref, and constructs the final SessionDefinition/Header/head together; no pre-gate or already-`COMMITTED` stale Header is accepted. Conversation Storage never publishes an entity, creates a generation marker, selects a Session directory, or returns a physical commit receipt. The Header remains creation provenance and file identity, not a definition/lifecycle log, current authorization proof or old execution recovery source.

On Store V1 restart, a missing `PUBLISHED` Session with an exact G1 staging shape is not a published Conversation Storage target. Before G1 `COMMITTED`, DurableState may check only the private regular-file/non-link/1-GiB physical metadata and must not parse opaque conversation bytes. With G1 `COMMITTED`, DurableState reconstructs the expected Header from the durable G1 head/definition and passes the already same-open observed `File` plus declared length to `validate_unpublished_conversation_for_recovery`: ordinary Sessions are Header-only; Fork children are canonical linear files, including Header-only. After the classifier returns, DurableState re-observes both the held handle and path and applies the same-open identity/length check, so the full classifier cannot silently accept a replaced or length-drifting conversation file. `TooLarge`, `Corrupt`, and `Unavailable` map back to the corresponding durable-open failures. These are current platform-observable facts; full Windows/NTFS identity, reparse, and native-process coverage remains the separate M5.0 pending platform gate. This strict restart classifier does not bind a source seed, validate an anchor against source bytes, make the unpublished child catalog-visible, or create or substitute for `PreparedConversationProof`; the latter remains only the publication readback proof for a newly written child.

No API provides `SessionWriter::append`, raw JSON append, caller-provided projection delta, middle-line repair, recording-failure rollback, segment resume, unrecorded-tail backfill or an execution permit derived from physical commit.

## 物理文件

物理encoding、field order、limits和bounded scanner的exact owner是[Conversation JSONL Format V1](../formats/conversation-jsonl-v1.md)。semantic overview：

```text
sessions/<SessionId>/conversation.jsonl
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

`StoredUserMessage`、`StoredAssistantContent`和`StoredToolMessage`是durable facts，不是`ModelMessage`或shadow transcript definitions。Storage只按其owning format保存/replay这些facts；live reducer和M5 projector必须调用Prompt-owned constructors/projections形成provider-neutral messages，不能在Storage/Wire内复制该shape。

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

Stored interactions只保留loaded execution中具有合法single producer、EntryId owner和historical conversation意义的facts。request/resolution使用Runtime public view同源的bounded safe types：Tool approval只保存redacted summary、safe options与selected index/kind，不保存private option→PermissionSet map；UserQuestion只保存non-secret Text/SingleChoice request和answer；Cancelled保存closed InteractionCancelReason。live reducer only derives these stored values from owner `InteractionRequest`/`ResolvedInteraction`; it never accepts a caller-built stored interaction body. 任意host Resolve（含HostCancelled）的resolution_key为Some，owner-driven closure为None。exact relation/wire见Format V1的[request](../formats/conversation-jsonl-v1.md#interaction-requested)与[resolution](../formats/conversation-jsonl-v1.md#interaction-resolved)。format v1没有secret/password/credential variant，raw secret不得进入JSONL。

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

Conversation Storage引用[Compaction-owned `StoredCompaction`与`StoredCompactionModelCall`](compaction.md#summary-validation与provenance)，只把该validated value装入`StoredEntryBody::Compaction`并按[Format V1 projection](../formats/conversation-jsonl-v1.md#compaction)serialize/replay。它不维护第二份Rust declaration或provenance semantic type。

automatic SummaryModel路径始终`model_call = Some`；summary<=65,536 bytes，finish/retry/request max/metadata使用Format v1 exact limits。

recorded marker只影响future replay。live Replace在inline record attempt前已经生效。marker缺失时restart恢复旧conversation；marker存在但无法在当前effective stable-unit projection上匹配index大于0的unit `first_entry_id`时，replay忽略并报告diagnostic。`None`只在当前effective conversation非空时表示覆盖全部units。

## History Tree

`EntryId + parent_id`形成recorded tree。ordinary live mutation的parent使用当前live selected head。由于Recorder只写连续前缀，正常failure不会主动写出缺失parent后的suffix。

cold file仍可能因手工编辑、旧bug或partial migration包含orphan；replay隔离而不是brick全部history。

不建立Branch entity。Replay按下列规则唯一选择effective path：

1. physical scan中第一个accepted且`parent_id = None`的entry成为canonical root；在它之前若无accepted entry则继续等待；
2. 后续`parent_id = None` entry不创建第二个current root，保留为isolated history node并产生`invalid_relation`；missing-parent orphan同样不属于canonical component；
3. accepted entry仅在其parent已经位于canonical component时加入该component；branch本身合法并保留；
4. chosen leaf是canonical component内physical scan ordinal最大的accepted entry。parent必须引用更早line，因此该entry在component内必为leaf；timestamp、EntryId lexical order和ItemId不参与；
5. selected path是canonical root到chosen leaf的唯一parent chain；`recorded_head = chosen_leaf`，new Recorder append、RecordedHistory Fork与sanitized projection都使用它；非selected branches仍可由history tree query观察；
6. 无accepted root时selected path与recorded_head均为空。collision guard仍seed全部first-valid accepted EntryId，包括isolated root/orphan/非selected branch，防止future identity reuse。

exact selection vectors见[branch-last-leaf](../fixtures/wire-v1/conversation/corruption/branch-last-leaf.jsonl)与[multiple-root-isolation](../fixtures/wire-v1/conversation/corruption/multiple-root-isolation.jsonl)。

## Tolerant Replay

**Canonical cross-module invariant: INV-002.**

replay按[Format V1 bounded decode](../formats/conversation-jsonl-v1.md#bounded-decode与replay)顺序：

1. 检查whole-file physical byte cap；超过1 GiB返回HistoryTooLarge，不truncate tail；
2. strict解析Header；oversized/malformed/invalid UTF-8/unsupported version立即fail Load；
3. 以bounded scanner读取newline-terminated entries并在scan中执行1,000,000 complete-entry cap；oversized line stream-discard到LF，invalid UTF-8/malformed/unknown variant skip + diagnostic；
4. required envelope/body/typed scalar decode完成后先校验Entry.session_id == Header.session_id；session mismatch line不进入collision guard、不能满足later parent；
5. 对session-valid candidate执行EntryId first-valid wins并立即seed collision guard；从此即使parent/Item/Tool/Interaction relation后来被隔离，该ID也不得复用；
6. UserMessage正文与解释性contribution stamps独立校验；未知origin、malformed stamp或越界`content_part_index`直接丢弃；同一part first valid stamp wins；合法正文保留；
7. missing parent形成isolated orphan root；
8. invalid Turn/Item/Tool/Interaction relation只隔离对应projection；
9. invalid Compaction marker忽略该effect并diagnose；
10. whole-file cap已通过时，最后unterminated bytes作为partial tail忽略；
11. 按History Tree规则选择recorded head，构建history view和sanitized conversation；
12. 每个diagnostic fact先递增per-code total；只保留前100条redacted detail，若有省略则追加`diagnostics_truncated` summary，记录omitted detail count与包含已保留facts在内的per-code totals。

Conversation Storage拥有closed replay diagnostic taxonomy，并与Wire Schema required codes使用同一exact snake_case strings；public `SessionDiagnosticView.code`不能透传parser/OS/provider text：

```rust
pub enum SessionReplayDiagnosticCode {
    PartialTail,
    OversizedLine,
    InvalidUtf8,
    MalformedJson,
    InvalidEntry,
    UnknownRecordVariant,
    UnknownEntryVariant,
    DuplicateEntryId,
    MissingParent,
    SessionMismatch,
    InvalidRelation,
    InvalidContributionStamp,
    DuplicateContributionStamp,
    InvalidToolExchange,
    InvalidInteractionRelation,
    InvalidCompactionMarker,
    DiagnosticsTruncated,
    HistoryTooLarge,
}
```

```text
partial_tail
oversized_line
invalid_utf8
malformed_json
invalid_entry
unknown_record_variant
unknown_entry_variant
duplicate_entry_id
missing_parent
session_mismatch
invalid_relation
invalid_contribution_stamp
duplicate_contribution_stamp
invalid_tool_exchange
invalid_interaction_relation
invalid_compaction_marker
diagnostics_truncated
history_too_large
```

Unknown additive object fields在structural caps内bounded skip且不产生diagnostic。每个complete line按scanner → bounded schema/variant → Header session match → duplicate identity → parent/domain relation → projection顺序确定primary code；同一失败stage不凭hash-map iteration选择code。一个line最多产生一个primary code，合法User body内独立drop的stamp分别归并为`invalid_contribution_stamp`或`duplicate_contribution_stamp`。Tool/Interaction/Compaction projection可以在entry被accepted为historical node后额外产生owning projection code。read-only ignore与exclusive-writable truncate都使用`partial_tail`，action只存在于typed internal detail；前100条detail之外追加一次`diagnostics_truncated` summary，aggregate counter表示全部observed facts而非仅omitted suffix。exact fixture expectations见[Wire V1 Fixtures](../fixtures/wire-v1/README.md)。

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
#[derive(Clone)]
pub(crate) struct LiveConversationView {
    revision: ConversationRevision,
    messages: Arc<[ModelMessage]>,
}

impl LiveConversationView {
    // Private to this owner module; called only by LiveSessionState.
    fn from_live_state(
        revision: ConversationRevision,
        messages: Arc<[ModelMessage]>,
    ) -> Self;

    pub(crate) fn revision(&self) -> ConversationRevision;
    pub(crate) fn messages(&self) -> &[ModelMessage];
}

pub(crate) struct ReplayedConversationView {
    recorded_head: Option<EntryId>,
    messages: Arc<[ModelMessage]>,
    diagnostics: Arc<[SessionReplayDiagnostic]>,
}
```

`LiveConversationView`是immutable `Clone` value：clone共享Arc-backed flattened message sequence，并保持revision、message semantic identity、order和provenance，不重建message。LiveSessionState只从canonical facts/stable units的immutable `ModelMessage` clones形成flattened view；它不从borrowed messages或caller-provided suffix重新投影。fields和`from_live_state()` constructor保持owner-module private；only `revision()` and `messages()` are exposed crate-wide. M4没有live/replay common trait：ordinary Prompt assembly consumes this live view only. `ReplayedConversationView` is an M5 storage reconstruction value, not a second M4 producer or Prompt input; M5 must define any second-producer interface explicitly before a generic trait exists.

`LiveCompactionSourceView`及stable-unit shape由[Compaction](compaction.md#stable-unit-source)语义拥有。`capture_conversation_views()`是唯一current live read seam，LiveSessionState在同一短guard内使用Compaction-owned validated factories形成source；factory的`CompactionSourceError`只在此caller boundary映射为owner-local `LiveConversationError`，而不会反向进入Compaction API，并同时形成ordinary `LiveConversationView`。reducer负责把ordinary User、无ToolCall Assistant、完整Assistant+ToolResults exchange和leading rolling summary分组成exact EntryId-bearing units；Compaction负责估算和cut。每个rolling summary unit的origin是安装它的外层StoredCompaction entry ID，Tool exchange只能以Assistant entry作为marker boundary。capture guard在planning或任何await前释放。

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

LiveSnapshot在一次短live-state critical section内完成：它从`LiveSessionState`保留的完整`Vec<Arc<StoredSessionEntry>>`捕获path，而不是从EntryId重查或重建entry：

```text
resolve public ForkAnchor
→ capture exact selected path
→ clone immutable recordable facts and stable IDs
```

anchor解析和selected path必须来自同一个snapshot。capture前已经apply的live mutation会被复制，即使对应`SessionRecorder.record().await`仍在执行；capture后才apply的mutation不会被复制。stream draft、ProgressEvent、pending queue value和其他未apply状态不属于snapshot。source guard必须在target staging I/O前释放。

RecordedHistory is captured as `RecordedForkConversationLease` from the tolerant replay selected path; it blocks Load, append, and tail truncate while the semantic stream runs. Both source forms preserve copied historical EntryId, parentId, TurnId, ItemId, RequestId, ToolCallId, timestamp, body, and selected order; only the child SessionId is new, and future entries use fresh IDs.

`stream_reencode_for_child(child_session_id, sink)` is never a raw source JSONL byte copy. Conversation Storage emits a new child Header and, for every selected `StoredSessionEntry`, validates and re-encodes its canonical JSONL with `session_id = child_session_id`; EntryId, parentId, TurnId, ItemId, RequestId, ToolCallId, timestamp, body, and selected order remain exactly preserved. DurableState owns the sink and publication; Conversation Storage owns Header/entry rewrite and validation. The canonical rule preserves every historical field. The ordinary durable fixture byte-compares every field actually present after only SessionId rebinding; because its two copied entries are User messages, it does **not** itself prove RequestId/ToolCallId handling. Wire conversation fixtures cover those body variants.

target使用final-path markerless staging + marker publication建立完整新record stream。Conversation Storage在`DurableCommitBarrier`前完成最多1 GiB的streaming re-encode、sync和bounded-memory full reread validation，返回`PreparedConversationProof`；selected path未全部materialize或replay validation失败时Fork返回typed error且不发布child，并清理exact markerless target。这一步是Fork publication前的完整semantic re-encode，不是已发布Session上的best-effort suffix recording。child后续和其他Session一样始终尝试记录。selected path可以在任意公开message anchor结束；child不继承source active Turn状态，发布后保持Unloaded，未来Load得到Idle conversation view。

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
receive DurableState PublishedConversationTarget and optional root-lease-derived writable proof
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

Tail处理只允许在valid v1 Header、whole-file cap通过且DurableState签发root-lease-derived `ExclusiveWritableConversationLease`后，截断final unterminated partial line到last LF。完整newline-terminated entry即使oversized、malformed、unknown或来自先前outcome-unknown write，也必须保留physical bytes；typed replay决定接受或隔离，不执行middle repair、reparent或gap marker synthesis。

## Diagnostics

```rust
pub enum SessionReplayDiagnostic {
    Detail(SessionReplayDiagnosticDetail),
    Truncated(SessionReplayDiagnosticsTruncated),
}

pub struct SessionReplayDiagnosticDetail {
    pub code: SessionReplayDiagnosticCode,
    pub entry_id: Option<EntryId>,
    pub line_number: Option<u64>,
    pub redacted_detail: Option<String>,
}

pub struct SessionReplayDiagnosticsTruncated {
    pub omitted_detail_count: u64,
    pub totals: Arc<[SessionReplayDiagnosticCount]>,
}

pub struct SessionReplayDiagnosticCount {
    pub code: SessionReplayDiagnosticCode,
    pub count: u64,
}
```

`Detail`最多100条并保持physical/emission order。`Truncated`仅在`omitted_detail_count > 0`时作为最后一个record存在；`totals`按exact snake_case code bytes升序、每code恰一项，count覆盖全部observed source facts（含前100条detail）。synthetic `Truncated` record本身不递增`diagnostics_truncated`或任何total。它是owner-side aggregate source，不得一对一append到public Vec；[Runtime Interface diagnostic projection](runtime-interface.md#diagnostic-projection-limits)按Snapshot/Query limit重新计算returned detail、omitted count和最后一个`diagnostics_truncated` view。u64 count使用checked increment；在1,000,000-entry/record structural caps下overflow是invariant failure。

Diagnostics exact code、100-detail cap、512-byte redacted message和aggregate规则由[Wire Schema](wire-schema.md#diagnostics)与[Format V1](../formats/conversation-jsonl-v1.md#bounded-decode与replay)拥有。禁止暴露raw line、credential、绝对敏感路径、完整Tool output、OS error或provider payload。

## 测试要求

- M4 reducer uses the deterministic sentinel/no-ID table in [Development Plan](../development-plan.md#m4-liveconversation-reducer): for every returned apply error class—revision overflow; entropy/collision; invalid body/relation/Turn; Prompt projection; source factory/stale/cross-session/identity; cut/marker/pending exchange; and Interaction conflict/key/family—snapshot and prove unchanged selected head, full path/Arc identities, revision and all reducer state, then prove the next valid apply receives the same first candidate. Pre-allocation validation asserts zero allocation calls; entropy/collision failure may call allocator but cannot reserve or advance the sentinel. `InteractionResolutionCandidate::host` rejects non-host origin and `owner_cancellation` rejects HostCancelled/non-cancellation before reducer invocation or allocation. Same-key/same-payload resolution separately proves zero calls/no entry/no path append and preserves that sentinel.
- Create的SessionHeader staging失败或publication前crash不产生catalog-visible Session；
- Create只写SessionHeader；definition/metadata/lifecycle mutation不生成StoredSessionEntry；
- Load始终尝试初始化Recorder，失败得到Degraded而不是Disabled；
- live owner在apply前分配EntryId，Recorder观察到exact same ID；
- EntryId allocation使用CSPRNG、unique candidate在return前reserve、最多32次collision retry；entropy/collision exhaustion返回redacted typed error，不panic且不改变state/head/revision；replay/Fork copied IDs均seed guard；
- `ConversationRevision`逐项覆盖Input/Steer、hidden ToolCall Assistant、complete-exchange promotion、partial/abandoned settlement、Interaction、progress/usage/recording、Compaction、failed/idempotent apply；checked overflow先于EntryId allocation；
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
