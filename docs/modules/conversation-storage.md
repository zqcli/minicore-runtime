# Conversation 与 SessionStorage 架构设计

状态：当前权威架构（ADR 0124后，生产实现待启动）
日期：2026-07-29

## 目的

本文定义MiniCore conversation与SessionStorage的durable ownership、逐entry JSONL、history tree、宽容replay、Tool exchange投影、Compaction、fork和restart recovery。

设计目标：

- SessionStorage保存已成功写入的conversation、message与durable lifecycle事实；
- live execution只写严格合法entry；
- 单行损坏或局部引用异常不brick整个Session；
- 模型只接收provider-valid conversation；
- branch、rewind和fork继续使用简单`EntryId + parent_id`树；
- 避免用大量nested EntryId把本地transcript提升为事务式执行证明系统。

本文不定义：

- SessionExecutor、SessionIngress和异步operation的完整实现；
- Runtime command/query/event/snapshot payload；
- provider adapter；
- Sandbox enforcement；
- power-loss durability、remote replication、多进程multi-writer；
- projection snapshot、segmentation、vacuum或blob store。

## 同类项目结论

| 项目 | Durable layout | 恢复与Identity |
| --- | --- | --- |
| pi | header + `id/parentId` entry tree | malformed line跳过；fork复制历史entry ID；ToolResult按ToolCallId关联 |
| Codex | typed rollout JSONL | parse error warning后继续；公开协议保留thread/turn/item/call ID |
| Gemini CLI | metadata/message JSONL | 单行解析错误忽略；message/tool-call ID用于rewind和更新 |
| OpenHands | base state + per-event JSON | event ID/parent ID树；action/tool-call ID关联结果 |
| Claude Code | session JSONL | 可观察到session UUID、entry UUID、parent UUID和tool-use ID |

MiniCore采用相同的宽松恢复基线，同时保留严格live writer和typed diagnostics。

## 决策摘要

- 每个Session一个JSONL文件；第一行为immutable SessionHeader；
- 后续一行一个`StoredSessionEntry`；完整换行行是process-crash可见单位；
- 一个writable Session同时最多一个`SessionWriter`；
- 所有mutation通过`SessionWriter::append(SessionEntryDraft)`；
- `EntryId + parent_id`形成history tree；
- `EntryId`由writer分配，ordinary append的parent是current entry；
- live append严格校验；cold replay跳过或隔离局部损坏并返回diagnostics；
- `source = Input`的UserMessage内联`StoredTurnStart`，不再有独立TurnContext entry；
- ledger不保存`ToolExecutionStarted`；
- ledger不保存`ToolRoundCompleted`；
- assistant ToolCall与Tool message按`TurnId + ItemId + ToolCallId`关联；
- complete Tool exchange由最后一个补齐集合的tool entry触发conversation delta；
- replay遇到incomplete/orphan/identity-conflicting Tool exchange时从模型conversation排除整个exchange；duplicate result first valid wins并告警；
- Compaction只保存rolling summary和`first_kept_entry_id`；
- Fork复制selected path并保留历史ID，只分配新SessionId；
- MVP不提供repair utility；
- cold load仍按完整文件O(n)线性扫描，不建设projection checkpoint/index。

## Ownership

```text
MiniCoreRuntime
└─ SessionStorage
   ├─ create/open/fork
   ├─ per-session SessionWriter
   ├─ strict append validation
   ├─ tolerant replay/projectors
   └─ private JSONL implementation
```

SessionStorage拥有：

- Session ledger文件与Header；
- EntryId allocation；
- current physical head和parent graph；
- strict live append；
- tolerant replay；
- conversation、Turn、Item、Interaction、Usage和history projections；
- partial-tail handling；
- replay diagnostics；
- fork selected-path copy。

SessionStorage不拥有：

- Model/Tool scheduling；
- current-Runtime Tool side-effect start状态；
- Prompt assembly；
- Tool permission、Sandbox或executor；
- Workspace authorization；
- AgentLoop；
- provider stream；
- UI repair workflow。

## 最小接口

当前只有一个backend，不建立`dyn SessionStorageAdapter` hierarchy。

```rust
pub(crate) struct SessionStorage {
    // private root/index/config
}

impl SessionStorage {
    pub async fn create(
        &self,
        create: CreateSessionLedger,
    ) -> Result<OpenedSessionLedger, SessionStorageError>;

    pub async fn open(
        &self,
        session_id: SessionId,
    ) -> Result<OpenedSessionLedger, SessionStorageError>;

    pub async fn replay_at(
        &self,
        session_id: SessionId,
        checkpoint: ConversationCheckpoint,
    ) -> Result<ReplayedSession, SessionStorageError>;

    pub async fn fork(
        &self,
        request: ForkSessionLedger,
    ) -> Result<OpenedSessionLedger, SessionStorageError>;
}

pub(crate) struct OpenedSessionLedger {
    pub replay: ReplayedSession,
    pub access: SessionLedgerAccess,
}

pub(crate) enum SessionLedgerAccess {
    Writable(SessionWriter),
    ReadOnly { reason: SessionStorageError },
}

pub(crate) struct ReplayedSession {
    pub projections: SessionProjections,
    pub diagnostics: Arc<[SessionReplayDiagnostic]>,
}

pub(crate) struct SessionWriter {
    // exclusive per-session writer state
}

impl SessionWriter {
    pub async fn append(
        &mut self,
        draft: SessionEntryDraft,
    ) -> Result<CommittedSessionEntry, SessionWriteError>;
}
```

`create()`成功必须返回Writable；`open()`在history可读但exclusive lease、recovery terminal或writer初始化失败时可以返回ReadOnly access。Session lifecycle仍可发布history/query view，但future Turn admission为Unavailable；调用方不能从ReadOnly access构造SessionExecutor writer。

不提供：

```text
append_raw_json
append_arbitrary_message
rewrite_middle_line
set_current_entry_without_append
automatic_reparent
repair_session
generic transaction callback
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
    pub format_version: u16,
    pub session_id: SessionId,
    pub created_at: Timestamp,
    pub fork_origin: Option<ForkOrigin>,
}

pub struct ForkOrigin {
    pub source_session_id: SessionId,
    pub source_entry_id: Option<EntryId>,
}
```

Header immutable。Session name、lifecycle、current definition pointer等entity metadata不通过重写Header更新。

每条entry使用单行compact JSON，字符串换行/control character必须escape。field order不承担语义。

## Entry Envelope

```rust
pub struct StoredSessionEntry {
    pub format_version: u16,
    pub entry_id: EntryId,
    pub parent_id: Option<EntryId>,
    pub timestamp: Timestamp,
    pub body: StoredEntryBody,
}

pub enum StoredEntryBody {
    Message(StoredMessage),
    Event(StoredDurableEvent),
    Compaction(StoredCompaction),
}
```

不再定义：

```text
StoredEntryBody::TurnContext
StoredDurableEvent::ToolExecutionStarted
StoredDurableEvent::ToolRoundCompleted
```

基础规则：

- `entry_id`由writer分配，caller不能预分配；
- ordinary append的`parent_id`等于writer current entry；
- history branch append可以选择已存在entry为parent并使新entry成为current；
- live writer拒绝duplicate ID、unknown parent和terminal后新work；
- cold replay将missing parent视为orphan root，不拒绝整个Session；
- EntryId、TurnId、ItemId、RequestId和ToolCallId保持typed，不混用；
- EntryId、TurnId、ItemId和RequestId在Session内唯一；Fork复制历史ID，因此跨Session不要求唯一；
- ToolCallId由ModelGateway adapter归一化：保留provider原生ID，缺失时生成response-local opaque ID；只要求同一assistant entry内唯一，durable关联使用`TurnId + ItemId + ToolCallId`。

```rust
pub struct SessionEntryDraft {
    expected_current_entry: Option<EntryId>,
    parent_entry: Option<EntryId>,
    body: PendingEntryBody,
}
```

`expected_current_entry`用于当前writer的乐观并发，不进入durable operation-key protocol。

## Message Entry

```rust
pub enum StoredMessage {
    User(StoredUserMessage),
    Assistant(StoredAssistantMessage),
    Tool(StoredToolMessage),
}
```

### User Message

```rust
pub struct StoredUserMessage {
    pub turn_id: TurnId,
    pub item_id: ItemId,
    pub source: StoredUserMessageSource,
    pub turn_start: Option<StoredTurnStart>,
    pub content: Arc<[UserContent]>,
    pub contribution_stamps: Arc<[StoredPromptContributionStamp]>,
}

pub enum StoredUserMessageSource {
    Input,
    Steer,
}
```

规则：

- `Input`必须携带`turn_start`，其append是领域Turn开始线性化点；
- `Steer`不得携带`turn_start`，必须绑定current Running Turn；
- FollowUp在下一Turn写新的Input；
- Interaction resolution不是UserMessage；
- contribution stamp只作历史provenance，不在cold replay时重新读取Workspace/Skill source或验证旧authorization。

### StoredTurnStart

```rust
pub struct StoredTurnStart {
    pub agent_id: AgentId,
    pub agent_revision: AgentRevisionRef,
    pub session_definition_revision: SessionDefinitionRevision,
    pub model: StoredModelDescriptor,
    pub workspace: StoredWorkspaceSummary,
    pub diagnostics: Arc<[StoredContextDiagnostic]>,
}

pub struct StoredModelDescriptor {
    pub provider_id: ProviderId,
    pub model_id: ModelId,
    pub generation: EffectiveGenerationPolicy,
}

pub struct StoredWorkspaceSummary {
    pub cwd: WorkspaceRelativeDisplay,
    pub roots: Arc<[WorkspaceRootDisplay]>,
}
```

当前MVP支持exact Agent pin和Session definition revision，因此live Input必须写入这两个exact ref；这些字段用于历史解释和UI，不是restart authorization input。cold replay不要求旧AgentDefinition、SessionDefinition、WorkspaceSnapshot或ModelDefinition仍可解析。

不保存：

```text
WorkspaceRevision
WorkspaceSnapshotRef
ModelDefinitionVersion
ModelExecutionRef
PromptSet / ToolSet / SkillView
credential / endpoint / auth principal
cancellation token / waiter / executor handle
```

### Assistant Message

一个finalized logical model response保存为一个assistant entry，stream delta不持久化。

```rust
pub struct StoredAssistantMessage {
    pub turn_id: TurnId,
    pub phase: StoredAssistantPhase,
    pub model: StoredModelDescriptor,
    pub response_id: Option<String>,
    pub content: Arc<[AssistantContent]>,
    pub usage: Option<ModelUsage>,
    pub finish_reason: ModelFinishReason,
    pub provider_metadata: StoredProviderResponseMetadata,
    pub retry_count: u32,
}

pub enum StoredAssistantPhase {
    Intermediate,
    Final { completion: TurnCompletion },
}

pub enum AssistantContent {
    Reasoning(StoredReasoning),
    Text {
        item_id: ItemId,
        text: String,
    },
    ToolCall {
        item_id: ItemId,
        tool_call_id: ToolCallId,
        name: ToolName,
        arguments: Value,
        index: u32,
    },
}
```

live规则：

- content顺序是canonical display与ToolCall顺序；
- 同一assistant entry内ItemId和ToolCallId唯一；
- 含ToolCall的assistant必须为Intermediate；
- 无ToolCall Intermediate用于Steer前的Assistant Continue；
- Final不能包含ToolCall；
- Final append完成Turn；
- retry_count遵守AgentRun 0–3限制；
- provider metadata只保存allowlisted bounded字段。

cold replay遇到违反这些规则的entry时忽略其conversation/Turn projection并报告diagnostic，不brick其他history。

### Tool Message

```rust
pub struct StoredToolMessage {
    pub turn_id: TurnId,
    pub item_id: ItemId,
    pub tool_call_id: ToolCallId,
    pub name: Option<ToolName>,
    pub source: ToolOutcomeSource,
    pub content: Arc<[ToolContent]>,
    pub is_error: bool,
}

pub enum ToolOutcomeSource {
    PreExecution,
    Executed,
}
```

规则：

- live append必须匹配同一selected path上的assistant ToolCall；
- ToolResult通过`TurnId + ItemId + ToolCallId`定位ToolInvocation；
- 同一个ToolCall最多一个Tool message；
- `PreExecution`用于validation/policy/approval deny/unavailable/cancelled-before-start；
- `Executed`表示executor已经运行并返回exact outcome；
- 不保存side-effect start entry reference；
- outcome unknown不生成Tool message，可以追加ToolAbandoned event或由terminal projection显示incomplete。

## Durable Event Entry

```rust
pub enum StoredDurableEvent {
    InteractionRequested(StoredInteractionRequested),
    InteractionResolved(StoredInteractionResolved),
    ToolAbandoned(StoredToolAbandoned),
    TurnInterrupted(StoredTurnInterrupted),
    TurnFailed(StoredTurnFailed),
}
```

### Interaction

```text
InteractionRequested
→ TurnId + ItemId + RequestId + typed request

InteractionResolved
→ TurnId + ItemId + RequestId + typed resolution + resolution_key
```

live顺序：

```text
request append/apply → notify host
resolution append/apply → wake waiter / continue Tool
```

cold replay：

- missing request的resolution被Interaction projection忽略并报告；
- duplicate/conflicting resolution first valid terminal wins；
- 其他conversation仍继续恢复。

### ToolAbandoned

```rust
pub struct StoredToolAbandoned {
    pub turn_id: TurnId,
    pub item_id: ItemId,
    pub tool_call_id: ToolCallId,
    pub reason: ToolAbandonReason,
}
```

ToolAbandoned表示当前Runtime知道该ToolInvocation不能继续且没有truthful ToolResult。它不进入模型conversation。live writer拒绝同一ToolInvocation同时出现Tool message与ToolAbandoned；cold replay采用first valid terminal outcome wins：ToolAbandoned先出现会永久使对应pending exchange不可完成，之后的Tool message只进入history inspection并产生diagnostic；Tool message先出现时，之后的ToolAbandoned被忽略并告警。restart也可以仅通过TurnInterrupted和incomplete Tool exchange投影表达旧工作无法恢复，因此recovery不要求为每个open call补写Abandoned。

### Turn Terminal

```text
TurnInterrupted → TurnId + typed reason + completed_at
TurnFailed      → TurnId + typed failure + completed_at
```

live writer仍要求一个Turn最多一个terminal fact。cold replay采用first valid terminal wins，之后属于同Turn的新work从Turn projection忽略并报告。

## Tool Exchange Projection

不建立ToolRound entity或durable completion marker。

assistant entry含ToolCall时，conversation projector创建pending exchange：

```rust
struct PendingToolExchange {
    assistant_entry_id: EntryId,
    turn_id: TurnId,
    expected: Arc<[ExpectedToolCall]>,
    outcomes: HashMap<ToolCallId, ProjectedToolOutcome>,
}

enum ProjectedToolOutcome {
    Result(StoredToolMessageRef),
    Abandoned,
}
```

`assistant_entry_id`只属于hot/replay projector内部索引，不写入其他entry。

`exchange-closing entry`定义为下一条合法User、Assistant、Compaction或Turn terminal；Interaction、progress和其他non-conversation event不关闭pending exchange。

当每个expected ToolCall在exchange-closing entry之前的first valid terminal outcome都是matching Tool message时，exchange完成；duplicate result仍由first valid result wins：

```text
assistant content
→ ordered tool messages by assistant call index
→ CommittedToolExchangeDelta
```

该delta同时：

- 将完整assistant/tool sequence加入CommittedConversationState；
- 将对应ToolInvocation Item投影为Completed；
- 允许SessionExecutor调用`AgentLoop::accept_committed_tool_results(...)`；
- 允许下一次模型调用。

以下情况使exchange不进入模型conversation：

- exchange-closing entry出现前仍missing ToolResult；
- orphan ToolResult；
- ToolCallId、ItemId或TurnId冲突；
- assistant entry本身被replay忽略；
- ToolAbandoned先于matching result出现，或outcome unknown。

同一个call出现duplicate ToolResult时，first valid result wins，后续duplicate从conversation/Item mutation忽略并产生diagnostic；它不撤销已经形成的complete exchange。replay不会合成ToolResult，也不会把部分calls/results发送给provider。后续合法User/Assistant entry仍可进入conversation。

## Compaction Entry

权威schema见[Compaction](compaction.md)。ConversationStorage只处理：

```rust
pub struct StoredCompaction {
    pub turn_id: Option<TurnId>,
    pub summary: Arc<str>,
    pub first_kept_entry_id: Option<EntryId>,
    pub model_call: Option<StoredCompactionModelCall>,
}
```

projection规则：

- marker必须指向当前selected path上已经model-visible的entry；
- summary替换marker之前的effective conversation prefix；
- marker及之后内容原样保留；
- marker为None时summary替换当时全部effective conversation；
- marker missing、指向ignored entry或不能形成provider-valid retained suffix时，忽略该Compaction并报告diagnostic；
- 后续Compaction可以再次摘要previous summary和新增内容。

## Strict Live Append

`SessionWriter.append()`在physical write前执行strict validation和projection preview：

```text
检查writer可用
→ 检查expected_current_entry
→ 检查parent_entry存在
→ 分配EntryId和timestamp
→ validate_live_entry(current projections, candidate)
→ 生成trusted projection delta
→ serialize candidate + '\n'
→ append/flush process-visible bytes
→ 更新current/index/projections
→ 返回CommittedSessionEntry
```

```rust
pub(crate) struct CommittedSessionEntry {
    entry: Arc<StoredSessionEntry>,
    previous_current_entry: Option<EntryId>,
    current_entry: EntryId,
    projections: CommittedProjectionDeltaSet,
}
```

live validator拒绝：

- stale current、invalid parent；
- duplicate Turn/Item/Request/ToolResult identity；
- Interaction family mismatch；
- Tool message找不到current path matching ToolCall；
- terminal后new work；
- invalid Compaction marker；
- entry超出`max_entry_bytes`。

apply只安装append前已生成的trusted delta；资源耗尽或hot checkpoint mismatch时丢弃hot projections并执行tolerant full replay。

## Append Error

```rust
pub enum SessionWriteError {
    StaleCurrentEntry,
    InvalidParent,
    InvalidEntry,
    EntryTooLarge,
    NotCommitted(StorageError),
    OutcomeUnknown,
    WriterUnavailable(StorageError),
}
```

- `NotCommitted`：physical write前失败，caller可以retry同一draft；
- `OutcomeUnknown`：physical write已开始但ack未知，poison writer并保守终结当前operation；
- 同一run不reopen/replay-by-key；
- 下次load按文件实际可见记录宽容replay；
- 不建立durable operation key或payload conflict index。

## Tolerant Replay

replay分为物理扫描和projection fold。

### 物理扫描

```text
read SessionHeader
→ scan every newline-terminated line
→ parse envelope/body
→ first valid EntryId wins
→ build parent index and physical order
→ collect diagnostics
→ choose last indexed entry as physical current head
```

处理规则：

| 问题 | replay行为 |
| --- | --- |
| final unterminated line | 忽略；writable lease下可truncate |
| invalid JSON | skip line |
| unknown core variant/version | skip line |
| duplicate EntryId | first wins，skip duplicate |
| missing parent | entry成为orphan root |
| forward parent | 当前entry成为orphan root；后续不回填 |
| invalid typed body | 对应entry skip |
| invalid cross-entry reference | relation/projection忽略，entry tree保留 |

### Selected Path

默认selected path从physical current entry沿parent向root回溯。遇到missing parent时停止，因此一个坏行后的有效suffix可以形成新的orphan-root path。

History query可以返回全部roots和branches，并标注orphan/ignored diagnostics。普通模型conversation只使用selected path。

### Projection Fold

每个projector独立应用best-effort规则：

- Conversation：只接受合法User、无ToolCall Assistant、complete Tool exchange、有效Compaction和Final Assistant；
- Turn：first valid Input starts，first valid terminal ends；
- Item：可解释的content进入，duplicate/conflict first wins；
- Interaction：request/resolution按RequestId关联，missing side忽略；
- Usage：只聚合合法assistant/compaction usage；
- Tree：保留所有成功parse且EntryId唯一的节点。

一个projector忽略entry不要求其他projector也删除它。例如orphan Tool message可以在history inspector显示，但不进入conversation或ToolInvocation projection。

## Replay Diagnostics

```rust
pub struct SessionReplayDiagnostic {
    pub severity: ReplayDiagnosticSeverity,
    pub line: Option<u64>,
    pub byte_offset: Option<u64>,
    pub entry_id: Option<EntryId>,
    pub reason: SessionReplayDiagnosticReason,
}
```

reason至少包括：

```text
MalformedJson
UnknownEntryVariant
DuplicateEntryId
MissingParent
InvalidEntryShape
InvalidReference
OrphanToolResult
DuplicateToolResult
ConflictingToolOutcome
IncompleteToolExchange
ConflictingTerminal
IgnoredCompaction
TailIncomplete
```

要求：

- diagnostic bounded、redacted；
- 不保存raw credential/header/payload；
- Host可以显示“部分历史未恢复”；
- warning本身不阻塞read-only history；
- safety-critical current Workspace/Agent/model无法resolve时，future Turn admission仍可Unavailable。

## Conversation Projection

```rust
pub(crate) struct CommittedConversationState {
    checkpoint: ConversationCheckpoint,
    messages: Arc<[MessageRecord]>,
}

pub(crate) struct ConversationCheckpoint {
    entry_id: Option<EntryId>,
}
```

checkpoint表示当前selected ledger head，不是内容hash。live append全部推进checkpoint，包括不改变messages的event。

```rust
pub(crate) enum ConversationChange {
    AdvanceOnly,
    Append(Arc<[MessageRecord]>),
    Replace { messages: Arc<[MessageRecord]> },
}

pub(crate) struct CommittedToolExchangeDelta {
    turn_id: TurnId,
    basis_checkpoint: ConversationCheckpoint,
    committed_checkpoint: ConversationCheckpoint,
    assistant_entry_id: EntryId,
    ordered_tool_call_ids: Arc<[ToolCallId]>,
    appended_messages: Arc<[MessageRecord]>,
}

pub(crate) struct CommittedSteerDelta {
    turn_id: TurnId,
    basis_checkpoint: ConversationCheckpoint,
    committed_checkpoint: ConversationCheckpoint,
    steer_entry_id: EntryId,
    appended_messages: Arc<[MessageRecord]>,
}
```

两个typed delta都只能由ConversationStorage的private constructor从committed receipts与当前selected-path projection构造：

- `basis_checkpoint`必须等于同一个AgentLoop最后接受的conversation basis；
- `committed_checkpoint`是触发delta时的current ledger head，允许跨过不改变model messages的Interaction/event entry；
- Tool delta的messages必须是一个assistant entry加按assistant call index排序的完整Tool messages；
- Steer delta的messages必须以同Turn的Steer UserMessage结束，可包含刚刚commit的Assistant Continue；
- incomplete/orphan/identity-conflicting exchange无法构造Tool delta；
- cold replay只生成新的`ConversationSeed`，不会把historical delta重新交给live AgentLoop。

`CommittedSessionEntry.projections`可以携带由当前append直接完成的Tool delta。Steer路径由ConversationStorage在Assistant Continue（如有）与Steer都commit后，根据committed receipts构造单个Steer delta；SessionExecutor不能自行拼装message vector或伪造checkpoint。

model-visible来源：

```text
User(Input/Steer)
Assistant Intermediate without ToolCall
complete Tool exchange
Assistant Final
valid Compaction
```

不直接消费：

```text
orphan/incomplete assistant ToolCall
orphan/duplicate Tool message
Interaction events
ToolAbandoned
TurnInterrupted / TurnFailed
streaming / retry / progress
```

Prompt只接受`CommittedConversationView`，并在provider lowering前再次断言没有unmatched ToolCall/ToolResult。

## Entry Tree 与 Current Entry

```text
Genesis
└─ e1 User(Input)
   └─ e2 Assistant
      ├─ e3 Tool result
      └─ e7 Historical branch
```

不建立Branch entity。

ordinary append：

```text
expected_current = e3
parent = e3
→ e4
```

history branch append：

```text
expected_current = e6
parent = e2
→ e7
```

physical current entry是最后成功append且可解析的unique EntryId。branch通过新append改变current，不修改旧entry。

## Fork

公开anchor仍使用UserMessage或Final Assistant Item语义，Runtime解析到selected path end。

fork流程：

```text
resolve anchor
→ read source selected root-to-anchor path
→ create target staging header with new SessionId/ForkOrigin
→ copy selected entries unchanged
→ tolerant full replay target staging
→ atomic publish target
→ create child Session entity
```

复制时保留：

```text
EntryId / parent_id
TurnId / ItemId / RequestId
ToolCallId
historical model/provenance payload
```

不执行nested identity remap。新append的EntryId及MiniCore生成的Turn/Item/Request ID使用fresh随机值；writer必须检查目标文件内EntryId不重复。ToolCallId继续按adapter normalization规则处理。

child不恢复：

```text
loaded state
provider session
AgentLoop
Tool task/waiter
Steer/FollowUp queue
old WorkspaceSnapshot或authorization cache
```

若复制path以unfinished Turn结束，target writable open按普通recovery中断旧Turn；history仍可读取。

## Recovery

process restart后：

```text
tolerant replay selected path
→ 不恢复provider stream、AgentLoop、Tool task、waiter或queue
→ 若无Running Turn：Ready/Idle
→ 若存在Running Turn：
     best-effort关闭Pending Interaction
     可选append ToolAbandoned或直接保留incomplete invocation
     append TurnInterrupted(HostRestart | RecoveryContextUnavailable)
→ apply receipts
→ Ready/Idle或typed Unavailable
```

recovery不：

- 自动重放Tool；
- 合成ToolResult；
- 补写ToolRound completion marker；
- 猜测丢失parent；
- 重建旧WorkspaceSnapshot/PromptSet/ToolSet/SkillView；
- 因无法写terminal而隐藏全部历史。

如果recovery append失败，Session history仍可read-only打开；future Turn admission可以标记Unavailable，等待用户新建/fork Session或解决storage问题。

## Corruption 与 Repair Policy

MVP不提供explicit repair utility。普通load只执行可逆的读取决策和final partial-tail截断，不修改中段内容。

禁止：

```text
automatic reparent
rewrite malformed middle line
invent missing ToolResult
merge duplicate IDs
semantic reconstruction
silent in-place repair
```

用户可以通过通用文件工具备份/检查原始JSONL；MiniCore只保证diagnostics足够定位line/offset和受影响entry。

## Performance

- cold open/recovery按完整文件O(n)扫描；
- loaded Session切换只路由现有SessionExecutionHandle；
- writer复用open file handle和buffer；
- 不以每entry `fsync`声称power-loss durability；
- 配置`max_entry_bytes`；
- streaming/progress不写ledger；
- usage aggregate、session list和search index是rebuildable cache；
- 没有真实规模数据前不建设ProjectionSnapshot、byte-offset index、segmentation或vacuum。

## 与 Session Execution 的关系

推荐执行流：

```text
append User(Input + StoredTurnStart)
→ AgentLoop NeedModel
→ ModelGateway.generate
→ append Assistant(Intermediate with ToolCall)
→ execute Tools under current-Runtime control
→ append one Tool message per truthful result
→ final matching Tool message produces CommittedToolExchangeDelta
→ AgentLoop accepts committed tool results
→ next model call
→ append Assistant(Final)
```

Ask-user：

```text
InteractionRequested append/apply
→ host notify
→ InteractionResolved append/apply
→ PreExecution ToolResult append
→ exchange complete后进入conversation
```

SessionWriter不决定何时调用Model、执行Tool、消费Steer、Cancel或terminalize Turn。

## 基础不变量

- 一个Session一个authoritative JSONL history tree；
- mutation只通过一个SessionWriter；
- live append严格、cold replay宽容；
- core IDs保持typed；Entry/Turn/Item/Request ID在Session内唯一，ToolCallId只在单assistant response内唯一；
- Fork保留复制历史ID；
- Input UserMessage内联TurnStart并开始Turn；
- Final Assistant或terminal event结束Turn；
- ToolCall与ToolResult通过TurnId/ItemId/ToolCallId关联；
- complete Tool exchange才进入模型conversation；
- incomplete exchange在replay中隔离并告警；
- ledger不保存ToolExecutionStarted或ToolRoundCompleted；
- Interaction request-before-notify、resolution-before-resume；
- Compaction使用summary + first-kept marker；
- corruption不brick整个Session；
- recovery不恢复旧I/O、不自动重放Tool、不合成结果；
- Prompt只消费sanitized CommittedConversationView。

## Test Matrix

至少覆盖：

- Input UserMessage与StoredTurnStart round-trip；
- Steer不能携带turn_start；
- strict live append拒绝missing parent/duplicate ToolResult/ToolResult与ToolAbandoned冲突/terminal后new work；
- malformed中段JSON被skip，后续valid line继续加载；
- duplicate EntryId first wins；
- missing parent形成orphan root；
- invalid body只影响对应projection；
- assistant多ToolCall、结果逆序完成，最后一个结果产生ordered Tool exchange delta；
- missing/orphan ToolResult不进入模型conversation；duplicate result first valid wins并告警；ToolResult/ToolAbandoned冲突采用first terminal wins；
- incomplete exchange被exchange-closing entry关闭，迟到result视为orphan；
- incomplete exchange后的新User/Assistant内容可恢复；
- Prompt输入无unmatched Tool protocol record；
- Interaction missing request/duplicate resolution diagnostics；
- final partial tail read-only ignore与writable truncate；
- Compaction marker valid/missing/ignored；
- Fork保留Entry/Turn/Item/Request/ToolCall IDs并分配新SessionId；
- Fork后新append的MiniCore-generated ID不发生collision；
- restart不恢复Tool task，Running Turn中断；
- recovery terminal append失败时history仍readable；
- full O(n) replay与hot projections在无corruption时得到相同模型conversation；
- corruption fixture返回bounded redacted diagnostics。

## 开放问题

实现阶段仍需冻结：

1. serde tags/field casing；
2. public ID和EntryId的UUID版本/文本格式；
3. `max_entry_bytes`；
4. replay diagnostic数量上限与聚合规则；
5. future format migration policy。
