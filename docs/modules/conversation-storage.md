# Conversation Recording 与 Replay 架构设计

状态：当前权威架构（ADR 0129后，生产实现待启动）
日期：2026-07-31

## 目的

本文定义MiniCore的live conversation、SessionRecorder、by-entry JSONL、recorded history tree、tolerant replay、recording health、fork和restart recovery。

核心定位：

- `LiveSessionState`是loaded Session的current-process truth；
- `SessionRecorder`把live mutation顺序inline append为可恢复的best-effort前缀；
- append outcome不作为Model、Tool、Interaction或StateEvent的durable correctness proof；
- cold replay只恢复已经record的完整行；
- JSONL不记录Turn start、terminal或其他execution lifecycle；
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

UUID/ULID算法、文本编码和serde wire由后续ID schema freeze决定。Recorder只验证entry identity已经存在且relation合法，不能创建、替换或规范化EntryId。

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
pub(crate) struct StagedSessionStorage {
    // unpublished target and validated initial SessionHeader
}

pub(crate) struct OpenedSession {
    pub replay: ReplayedSession,
    pub recorder: Arc<SessionRecorder>,
}
```

`StagedSessionStorage`只能由Agent/Session lifecycle publication路径消费，不能被query、Load或Turn admission观察。

`stage_create()`写入initial SessionHeader但不创建SessionRecorder；Agent/Session lifecycle只在该staging与SessionDefinition都成功后原子发布`Open + Unloaded`Session。header staging失败使Create失败，partial staging target不进入catalog。

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

```text
sessions/<SessionId>.jsonl
```

```text
line 1   SessionHeader
line 2+  StoredSessionEntry
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
    pub turn_id: Option<TurnId>,
    pub timestamp: Timestamp,
    pub body: StoredEntryBody,
}
```

```rust
pub enum StoredEntryBody {
    Message(StoredMessage),
    Event(StoredEvent),
    Compaction(StoredCompaction),
}
```

每行immutable。EntryId由live owner在apply前分配并进入collision guard，Recorder不能创建或改写。

## Stored Message

```rust
pub enum StoredMessage {
    User(StoredUserMessage),
    Assistant(StoredAssistantMessage),
    Tool(StoredToolMessage),
}
```

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
    pub item_ids: Arc<[ItemId]>,
    pub disposition: AssistantDisposition,
    pub content: Arc<[AssistantContent]>,
    pub model: ModelResponseSummary,
    pub usage: Option<ModelUsage>,
    pub logical_retry_count: u8,
}
```

```rust
pub enum AssistantContent {
    Reasoning(StoredReasoning),
    Text(String),
    ToolCall(StoredToolCall),
}
```

```rust
pub struct StoredToolCall {
    pub item_id: ItemId,
    pub tool_call_id: ToolCallId,
    pub name: ToolName,
    pub arguments: JsonValue,
}
```

```rust
pub struct StoredToolMessage {
    pub item_id: ItemId,
    pub tool_call_id: ToolCallId,
    pub outcome: StoredToolOutcome,
}
```

Tool durable correlation继续使用`TurnId + ItemId + ToolCallId`。ToolCallId只要求同一assistant response内唯一。

## Stored Events

```rust
pub enum StoredEvent {
    InteractionRequested(StoredInteractionRequest),
    InteractionResolved(StoredInteractionResolution),
    SessionDefinitionChanged(StoredSessionDefinitionChange),
    SessionLifecycleChanged(StoredSessionLifecycleChange),
}
```

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

recorded marker只影响future replay。live Replace在inline record attempt前已经生效。marker缺失时restart恢复旧conversation；marker存在但无法安全应用时replay忽略并报告diagnostic。

## History Tree

`EntryId + parent_id`形成recorded tree。ordinary live mutation的parent使用当前live selected head。由于Recorder只写连续前缀，正常failure不会主动写出缺失parent后的suffix。

cold file仍可能因手工编辑、旧bug或partial migration包含orphan；replay隔离而不是brick全部history。

不建立Branch entity。selected path由root到chosen leaf确定。

## Tolerant Replay

**Canonical cross-module invariant: INV-002.**

replay顺序扫描newline-terminated records：

1. 解析Header；
2. 对每个完整line解析typed entry；
3. UserMessage正文与解释性contribution stamps独立校验；未知origin、malformed stamp或越界`content_part_index`直接丢弃；同一part的stamp按recorded顺序first valid wins，后续重复丢弃并告警；合法正文始终保留；
4. duplicate EntryId使用first valid wins并告警；
5. malformed body跳过；
6. missing parent形成isolated orphan root；
7. invalid Session/Turn/Item/Interaction relation只隔离对应projection；
8. 最后partial line忽略并报告；
9. 构建recorded tree、history view和sanitized conversation；
10. 返回bounded redacted diagnostics。

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

## Recovery

```text
open file and attempt writable lease
→ tolerant replay recorded prefix
→ writable open truncates only final unterminated partial tail ignored by replay
→ create LiveSessionState from replay
→ sanitize incomplete exchange
→ initialize new inline recorder at replayed recorded head
→ resolve current Workspace/Agent definition
→ current_turn = None
→ SessionExecutor Idle or WorkspaceUnavailable
```

recording unavailable不阻止admission。新loaded instance始终尝试初始化Recorder，并根据open结果初始化为Healthy或Degraded；它不继承旧loaded instance的health object。Unload/Load永久丢弃旧unrecorded live tail和旧TurnStatus。Load不推断旧Turn outcome、不创建restart interruption，也不执行recovery append。

Tail处理只允许截断final unterminated partial line。完整newline-terminated entry即使来自先前outcome-unknown write，只要typed replay有效就必须保留；malformed中段行继续由tolerant replay隔离，不执行repair、reparent或gap marker synthesis。

## Diagnostics

```rust
pub struct SessionReplayDiagnostic {
    pub code: SessionReplayDiagnosticCode,
    pub entry_id: Option<EntryId>,
    pub line_number: Option<u64>,
    pub redacted_detail: Option<String>,
}
```

Diagnostics必须有数量上限、聚合重复错误、限制字符串长度，并禁止暴露credential、绝对敏感路径、完整Tool output或raw provider payload。

## 测试要求

- Create的SessionHeader staging失败或publication前crash不产生catalog-visible Session；
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
- malformed、unknown或越界contribution stamp被丢弃；同一part重复stamp first valid wins；合法UserMessage正文继续恢复；
- complete/incomplete/duplicate/conflicting Tool exchange；
- compaction marker missing/invalid；
- fork历史ID保留和future ID无collision；
- loaded Fork包含snapshot capture前已apply的unrecorded tail，unloaded Fork只复制RecordedHistory；
- live mutation apply后、record attempt返回前Fork仍使用LiveSnapshot包含该mutation；
- Fork与Unload竞态产生稳定且公开的LiveSnapshot或RecordedHistory source；
- live Fork staging失败不发布partial child；
- Fork不追加fork-specific terminal，child不继承source current Turn；
- restart不恢复execution objects；
- restart后current_turn为空且不合成Turn terminal；
- bounded redacted diagnostics；
- LiveConversation与无corruption replay产生相同sanitized messages。

## 开放问题

EntryId算法/文本wire、max entry bytes、diagnostic总量上限和format migration仍需freeze。EntryId owner由Q9关闭，Turn lifecycle omission与无closure recovery由Q10/ADR 0127关闭，Prompt contribution schema由ADR 0129关闭；Recorder Q1–Q10均已关闭，结论见[独立review](../review/async-loop-best-effort-recording-open-questions.md)。
