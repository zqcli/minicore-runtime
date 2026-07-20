# Conversation 与 SessionStorage 架构设计

状态：目标架构已确定；Session execution、compaction 和公开 protocol integration 待后续阶段完成
日期：2026-07-16

## 目的

本文定义 MiniCore conversation 与 SessionStorage 的 durable ownership、唯一写入 seam、atomic batch、JSONL 物理格式、projection、branch/fork、compaction、reload、corruption 和 recovery 语义。

本文重点解决：

- SessionStorage 如何成为唯一 durable truth；
- 哪些事实可以进入模型 conversation；
- operational Tool/Interaction facts 如何先持久化而不污染模型上下文；
- 一个 complete ToolRound 如何 all-or-none promotion；
- 一个逻辑 commit 如何映射到物理 JSONL；
- `EntryId`、`ItemId`、`ToolCallId` 和 fork identity 的关系；
- hot conversation projection 如何只应用成功 commit delta；
- branch、current leaf 和 stable checkpoint；
- compaction 如何改变 projection 而不改写历史；
- process crash、partial tail、corruption 和 commit outcome unknown 如何处理。

本文不提前冻结：

- Session execution actor/task/mailbox 的具体实现；
- Runtime command、query、event 和 snapshot payload；
- compaction summary prompt、token budget 和 provider adapter；
- Runtime-global Session catalog 的物理数据库；
- power-loss durability、remote replication 或多进程 multi-writer；
- large payload blob store、physical vacuum 和 retention policy；
- SQLite、object store 或 content-addressed backend。

## 同类项目结论

| 项目 | Storage 形状 | 值得借鉴 | 需要避免 |
| --- | --- | --- | --- |
| Codex | typed rollout JSONL；TurnContext、ResponseItem、EventMsg 和 Compacted replacement history；SQLite 作为可重建 metadata index | append log、log/index 分离、compaction checkpoint、reconstruction | record/compatibility 类型过多；incremental record 不提供 MiniCore complete ToolRound batch 语义 |
| pi | 一个 JSONL header + `id/parentId` entry tree；message、compaction、branch summary 都是 entry | 单文件、inspectable、branch tree 简单 | 每条 Message 独立 append；ToolCall/ToolResult 可 partial；approval 和 side-effect barrier 不 durable；rewrite 非 crash-atomic |
| Grok Build | chat_history、updates、summary 和多个 sidecars；Persistence actor；temp+rename；Committed/NotCommitted error | single writer actor、atomic rewrite、区分 commit outcome、replay checkpoint | 两个 conversation-like logs、多 sidecar ownership、synthetic repair 和较重 operational surface |
| Claude Code | JSONL uuid/parentUuid；assistant tool_use 与 tool_result 独立记录；file checkpoint 混入 session | resume/fork/checkpoint 产品体验、显式 parent link | partial ToolRound；conversation 与 Workspace file history 过度耦合 |
| Cursor | 内部格式未公开 | checkpoint/rewind 产品行为 | 不依据不可验证实现决定 MiniCore ownership |

MiniCore 采用：

```text
pi 的单文件 tree simplicity
+ Codex 的 log/index separation
+ Grok 的 commit-outcome discipline
+ MiniCore atomic SessionWriteBatch / committed-only promotion
```

## 决策摘要

已经确定：

- SessionStorage 是 Session conversation/execution ledger 的唯一 durable truth；
- 每个 Session 同时最多一个 SessionWriter；
- 已创建 Session 的全部 ledger mutation 通过 `SessionWriter::commit(SessionWriteBatch)`；
- Runtime code 不直接 append JSONL、分配 EntryId、移动 current leaf 或构造 committed projection；
- MVP 使用一个 Session 一个 append-only JSONL batch log；
- SessionHeader 是第一行，由 create/fork staging 原子写入；
- 后续每一物理行是一个完整 `StoredSessionBatch`；
- 一行是 process-crash atomic visibility unit；
- batch 内 record 顺序稳定，writer 分配 EntryId；
- operation/idempotency key 解决 commit acknowledgement 丢失和 outcome unknown；
- `parent_leaf` 形成同一 Session 内的 immutable branch tree；
- 不建立 Branch entity 或 BranchId；
- current leaf 是最后成功 commit batch 的 resulting leaf；
- operational facts 与 conversation promotion facts 保存在同一个 log；
- conversation projector 只消费 TurnStart、Steer、complete ToolRound、Compaction 和 final AgentMessage；
- complete ToolRound promotion 与 ToolInvocation Completed transitions 同 batch；
- hot `CommittedConversationState` 只应用 commit receipt 返回的 trusted delta；
- projection mismatch 时 reload，不猜测 patch；
- compaction append overlay，不改写或删除旧 records；
- user-facing branch/fork 只使用 stable checkpoint；
- fork baseline 使用 staging deep copy + identity remap + atomic publication；
- 只忽略/截断最后一个未换行的 partial tail；
- newline-terminated invalid batch 是 corruption，不能静默 skip；
- recovery 不生成 synthetic ToolResult，不自动重放 outcome-unknown Tool；
- projection snapshot、session index 和 search database 都只是 rebuildable cache；
- 当前不引入通用 Storage trait hierarchy、WAL layer、content-addressed DAG 或双 authoritative log。

## Ownership

```text
MiniCoreRuntime
└─ SessionStorage
   ├─ create/open/fork
   ├─ per-session SessionWriter
   │  └─ commit(SessionWriteBatch)
   ├─ replay / projection rebuild
   └─ private JSONL implementation
```

SessionStorage 拥有：

- Session ledger file；
- EntryId / BatchId allocation；
- operation key index；
- current leaf 和 branch graph reconstruction；
- batch validation；
- durable append；
- replay/projector；
- partial-tail handling；
- explicit corruption diagnostics；
- fork staging copy。

SessionStorage 不拥有：

- Agent/Session execution scheduling；
- Prompt assembly；
- Tool permission 或 execution；
- Workspace authorization；
- provider stream / AgentLoop；
- pending waiter；
- Runtime public transport；
- physical file checkpoint/Workspace rollback。

AgentDefinition 和 SessionDefinition head 的最终 entity-store shape 留给 Runtime interface/storage integration；本文只固定 Session conversation/execution ledger。

## 最小接口

当前只有一个真实 backend，因此不建立 `dyn SessionStorageAdapter` 或 Factory trait。

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
    pub writer: SessionWriter,
}

pub(crate) struct SessionWriter {
    // exclusive per-session writer state
}

impl SessionWriter {
    pub async fn commit(
        &mut self,
        batch: SessionWriteBatch,
    ) -> Result<CommittedSessionBatch, SessionWriteError>;
}
```

不提供：

```text
append_raw_record
append_message
set_current_leaf
write_projection
replace_history
save_runtime_state
generic transaction callback
```

`SessionWriteBatch` 字段私有，只能由 crate-internal validated constructors/builders 构造。

## 唯一写入 Seam

所有 Session ledger durable mutation 最终进入：

```text
SessionWriter::commit(SessionWriteBatch)
```

业务 helper 可以存在：

```text
commit_turn_start(...)
commit_interaction_request(...)
commit_tool_outcomes(...)
commit_complete_tool_round(...)
commit_terminal_turn(...)
```

但它们不能写 storage，只能构造 validated batch并调用 commit。

物理 `append_batch_line()` 是 SessionStorage private implementation；Session execution、ToolService、PromptService、command handler 和 event publisher都不能直接调用。

### 例外

只有两个非普通 append 路径：

- `SessionStorage::create()` 原子创建 SessionHeader；
- `SessionStorage::fork()` 写 staging target并原子 publish。

create/fork 都必须先写 newline-terminated staging file、flush 并完整 replay 验证，再以 exclusive create/atomic rename publish；它们不构成 runtime mutation 的第二 writer seam。

## 物理文件

MVP layout：

```text
sessions/<SessionId>.jsonl
```

```text
line 1   SessionHeader
line 2+  StoredSessionBatch
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
    pub source_checkpoint: ConversationCheckpoint,
}
```

Header immutable。Session name、lifecycle、current definition pointer 等 mutable entity metadata 不通过重写 Header 更新。

## StoredSessionBatch

一个物理 JSONL line 编码一个完整 batch。writer 必须使用 compact single-line JSON serialization：禁止 pretty print，字符串中的换行/control characters 必须 JSON escape，record 之间不能产生物理 newline：

```rust
pub struct StoredSessionBatch {
    pub format_version: u16,
    pub batch_id: BatchId,
    pub operation_key: IdempotencyKey,
    pub batch_fingerprint: BatchFingerprint,
    pub purpose: SessionCommitPurpose,
    pub expected_current_leaf: Option<LeafId>,
    pub parent_leaf: Option<LeafId>,
    pub committed_at: Timestamp,
    pub records: Vec<StoredRecord>,
}

pub struct StoredRecord {
    pub entry_id: EntryId,
    pub body: StoredRecordBody,
}

pub struct LeafId(EntryId);
```

基础规则：

- records 必须非空；
- Vec order 是 batch 内唯一 authoritative order；
- 最后一个 record 的 EntryId 生成 resulting LeafId；
- 只有 `None` 或已提交 batch 的 resulting LeafId 可以作为 parent/checkpoint；interior EntryId 永远不能成为 leaf；
- parent_leaf 是该 batch 的 logical parent；
- batch_id 是 private commit diagnostics/snapshot handle，不是领域 identity、branch identity 或 public protocol ID；
- operation_key 标识 caller intent/retry；
- batch_fingerprint 防止同 key 不同 payload；它覆盖 format version、purpose、expected_current_leaf、parent_leaf 和 ordered normalized logical records；只排除 envelope BatchId、各 record 自身新分配的 entry_id、committed_at 和 serialization details，payload 内引用的 EntryId/LeafId 必须参与；
- EntryId 由 writer 分配，caller 不能预分配；
- JSON object field order 不承担语义；
- format_version 显式控制当前 typed payload shape；baseline 只实现当前版本和必要的直接读取兼容，不建立通用 migration framework。

当前不增加 physical hash chain、Merkle tree 或 per-line cryptographic checksum。需要 bit-rot 检测时可以在 private file format 中增加 framing/checksum，不改变 SessionWriter interface。

## SessionWriteBatch

```rust
pub struct SessionWriteBatch {
    expected_current_leaf: Option<LeafId>,
    parent_leaf: Option<LeafId>,
    operation_key: IdempotencyKey,
    purpose: SessionCommitPurpose,
    records: Vec<PendingRecord>,
}
```

`expected_current_leaf` 是 CAS precondition；`parent_leaf` 是新 branch parent：

```text
ordinary append
→ parent_leaf = expected_current_leaf

branch append
→ expected_current_leaf = 当前 durable leaf
→ parent_leaf = 已验证 stable historical leaf
```

commit 成功后 current leaf 变为 batch resulting leaf。

## Commit Purpose

Purpose 用于 validation、diagnostics 和 replay compatibility，不是 entity：

```rust
pub enum SessionCommitPurpose {
    TurnStart,
    ToolOperations,
    Interaction,
    ToolOutcome,
    CompleteToolRound,
    Steer,
    Compaction,
    TurnTerminal,
    Recovery,
}
```

不为 purpose 建立 ID、CRUD、registry 或 lifecycle。

## StoredRecordBody

阶段 5 固定最小 immutable fact family：

```rust
pub enum StoredRecordBody {
    TurnStarted(TurnStartedRecord),

    ToolInvocationStarted(ToolInvocationStartedRecord),
    ToolExecutionStarted(ToolExecutionStartedRecord),
    ToolOutcomeKnown(ToolOutcomeKnownRecord),
    ToolInvocationClosed(ToolInvocationClosedRecord),

    InteractionRequested(InteractionRequestedRecord),
    InteractionResolved(InteractionResolvedRecord),

    ToolRoundPromoted(ToolRoundPromotedRecord),
    SteerCommitted(SteerCommittedRecord),
    CompactionCommitted(CompactionCommittedRecord),
    TurnTerminal(TurnTerminalRecord),
}
```

稳定 UserMessage、AgentMessage 和 Reasoning values 嵌入真正拥有其 atomic semantics 的 record：

- initiating UserMessage 在 TurnStarted；
- Tool-producing model output 的 AgentMessage/Reasoning 在 ToolRoundPromoted；
- Steer UserMessage 在 SteerCommitted；
- final AgentMessage 在 completed TurnTerminal。

不增加通用 `MessageAppended`，避免调用方绕过 Turn/ToolRound/terminal validation。

未来新增 record variant 必须代表真实 durable fact；stream delta、retry attempt、cache event 和 phase transition不能因为“方便记录”进入该 enum。

## Record 关系

### TurnStarted

```text
TurnStartedRecord
├─ TurnId
├─ exact execution metadata
└─ initiating UserMessage ItemId + CanonicalUserMessage
```

TurnStart batch 必须只有一个新 Turn，且 start 前 Session 没有 Running Turn。

### ToolInvocationStarted

```text
ToolInvocationStartedRecord
├─ TurnId
├─ ItemId
├─ ToolCallId
├─ model-emitted ToolName / requested arguments
└─ source_call_index
```

record durable/UI-visible，但不向 model-visible transcript 添加消息。它是 model-visible ToolCall name/requested arguments/source order 的唯一 canonical durable source；后续 promotion 不重复保存或改写 ToolCall payload。它不冒充 preflight 后的 resolved/frozen execution intent。

### InteractionRequested / Resolved

```text
InteractionRequested
→ RequestId + parent ItemId + typed request + expires_at

InteractionResolved
→ RequestId + typed resolution + durable resolution_key
```

Requested 未 commit 前不能 notify host；Resolved 未 commit 前不能 wake waiter或执行受保护副作用。

### ToolExecutionStarted

必须在可能发生外部副作用前 commit：

```text
ItemId
ToolCallId
resolved ToolName
frozen execution invocation fingerprint
requirements fingerprint
final authorization fingerprint
started_at
```

fingerprint 覆盖 preflight 后的 resolved Tool、frozen normalized arguments 和 execution requirements；exact executor implementation identity/content-reference 细节由 Tool 子系统后续闭合。该 record 证明“准备执行哪一个 side effect intent”，但 baseline recovery 仍不自动重放。

### ToolOutcomeKnown

保存 exact truthful ToolResult：

```text
ItemId
ToolCallId
source:
  PreExecution(validation / policy / approval / unavailable)
  | Executed { execution_started_entry_id: EntryId }
ToolResult
completed_at
```

它是 operational truth，不自动完成 Item，也不向 model-visible transcript 添加消息。Executed outcome 必须引用同一 Item/ToolCallId 的 ToolExecutionStarted；PreExecution outcome 禁止伪造 execution-start reference。

### ToolInvocationClosed

```rust
pub struct ToolInvocationClosedRecord {
    pub item_id: ItemId,
    pub tool_call_id: ToolCallId,
    pub closure: StoredToolInvocationClosure,
}

pub enum StoredToolInvocationClosure {
    Completed {
        outcome_entry_id: EntryId,
    },
    Abandoned {
        reason: ToolAbandonReason,
    },
}
```

Completed 必须引用同一 Session、同一 Item、matching ToolCallId 的 ToolOutcomeKnown。若 outcome source 为 Executed，其 execution_started_entry_id 也必须 exact match；PreExecution outcome 则不要求 ToolExecutionStarted。

### ToolRoundPromoted

```text
ToolRoundPromotedRecord
├─ TurnId
├─ optional stable AgentMessage text ItemId + content（不重复内嵌 ToolCall）
├─ optional Reasoning ItemId + content
└─ ordered members:
   ItemId + ToolCallId + outcome EntryId
```

normal complete ToolRound batch 必须同时包含：

```text
ToolInvocationClosed(Completed)*
+ exactly one ToolRoundPromoted
```

conversation projector只在该 promotion record committed 后看见 assistant/tool call/tool result sequence。promotion validation 必须按 ItemId 读取 canonical ToolInvocationStarted，并证明 ToolCallId、source order 和 outcome reference exact match；任何重复/漂移都 fail closed。

### SteerCommitted

保存 expected Running TurnId，以及由 PromptSet 规范化的 UserMessage ItemId/content。

### CompactionCommitted

保存：

```text
source ConversationCheckpoint（包含 TranscriptFingerprint）
summarized-through boundary
summary
retained-from boundary / retained references
```

cut 不能拆分 TurnStart、complete ToolRound 或其他 model-visible atomic unit。

### TurnTerminal

```rust
pub struct TurnTerminalRecord {
    pub turn_id: TurnId,
    pub terminal: StoredTurnTerminal,
}

pub enum StoredTurnTerminal {
    Completed {
        final_agent_message_item_id: ItemId,
        final_agent_message: AgentMessageItem,
        completed_at: Timestamp,
    },
    Interrupted {
        reason: TurnInterruption,
        completed_at: Timestamp,
    },
    Failed {
        failure: TurnFailure,
        completed_at: Timestamp,
    },
}
```

Completed record 同时保存 final AgentMessage ItemId/content。terminal batch 必须关闭所有 Pending Interaction 和 Started ToolInvocation。

## Atomic Commit Units

| Domain 时机 | 同 batch records | Conversation change |
| --- | --- | --- |
| Turn start | TurnStarted | Append initiating UserMessage |
| ToolCalls stable | ToolInvocationStarted* | AdvanceOnly |
| Interaction request | InteractionRequested | AdvanceOnly |
| Interaction response/timeout | InteractionResolved | AdvanceOnly |
| Tool side effect barrier | ToolExecutionStarted* | AdvanceOnly |
| exact Tool results | ToolOutcomeKnown* | AdvanceOnly |
| complete ToolRound | ToolInvocationClosed(Completed)* + ToolRoundPromoted | Append complete assistant/tool sequence |
| Steer | SteerCommitted | Append UserMessage |
| Compaction | CompactionCommitted | Replace projection |
| Turn completed | TurnTerminal(Completed with final AgentMessage) | Append final AgentMessage |
| Turn interrupted/failed | closures* + TurnTerminal | AdvanceOnly |
| Recovery | InteractionResolved* + ToolInvocationClosed* + TurnTerminal(Interrupted) | AdvanceOnly |

多个同类 operational records 可以合并到一个 batch，例如并行 Tool outcomes；不要求每个 Tool 单独 physical append。

## Commit Algorithm

```text
SessionWriter.commit(batch)
→ 检查 writer 未 poisoned / closed
→ 先检查 operation_key
   ├─ same key + same fingerprint：返回原 receipt，不再执行 leaf CAS
   └─ same key + different fingerprint：conflict
→ 检查 expected_current_leaf
→ 验证 parent_leaf 是 None 或 prior batch-result LeafId，拒绝 interior EntryId
→ 分配 BatchId / EntryId
→ 验证 record references 和 domain transitions
→ 计算 trusted conversation change
→ serialize one complete StoredSessionBatch + '\n'
→ append + flush
→ 更新 writer current leaf / in-memory indexes
→ 返回 CommittedSessionBatch
```

commit 一旦进入 physical append，不接受 Turn cancellation。

## CommittedSessionBatch

```rust
pub(crate) struct CommittedSessionBatch {
    batch_id: BatchId,
    entries: Arc<[StoredRecord]>,
    previous_leaf: Option<LeafId>,
    current_leaf: LeafId,
    projections: CommittedProjectionDeltaSet,
}

pub(crate) struct CommittedProjectionDeltaSet {
    conversation: CommittedConversationDelta,
    // private typed Turn/Item/Interaction/Recovery/branch deltas
}
```

CommittedSessionBatch 字段不直接暴露；只提供 diagnostics/read-only accessors 和 storage-owned apply_committed。全部 projection deltas 由 SessionStorage trusted projectors 生成，不接受 caller-provided delta。Session execution 必须先通过 storage-owned `apply_committed(receipt)` 应用 required projections，随后才能 publish event、notify request 或 wake waiter。

apply_committed 必须 all-or-rebuild：任何 projection base/checkpoint 不匹配都丢弃相关 hot projections并从 durable current leaf replay；不能只推进 Conversation 而让 Turn/Item/Interaction 停留在旧 leaf。

成功 receipt 是 runtime 应用 projection 和发布事件的唯一依据。

## Commit Error

```rust
pub enum SessionWriteError {
    StaleLeaf,
    OperationConflict,
    InvalidBatch,
    BatchTooLarge,
    NotCommitted(StorageError),
    OutcomeUnknown {
        operation_key: IdempotencyKey,
    },
    WriterUnavailable(StorageError),
}
```

语义：

- `NotCommitted`：已证明 batch 不存在，可以安全 retry；
- `OutcomeUnknown`：必须 reopen/replay 或按 operation_key lookup；它表示 storage commit acknowledgement 不确定，不等于 Tool side-effect outcome unknown，也不能直接派生 ToolInvocation Abandoned；
- `WriterUnavailable`：writer poisoned，不能继续 append；
- commit 后 hot projection apply failure 不回滚 durable batch，Session execution reload。

不需要 Grok 式“log committed 但 summary bookkeeping 失败”，因为 baseline commit path 不更新 authoritative sidecar。

## Conversation Projection

```rust
pub(crate) struct CommittedConversationState {
    checkpoint: ConversationCheckpoint,
    messages: Arc<[MessageRecord]>,
}

pub(crate) struct ConversationCheckpoint {
    leaf: Option<LeafId>,
    transcript_fingerprint: TranscriptFingerprint,
}

pub(crate) struct CommittedConversationView<'a> {
    checkpoint: &'a ConversationCheckpoint,
    messages: &'a [MessageRecord],
}

impl CommittedConversationState {
    pub(crate) fn view(&self) -> CommittedConversationView<'_>;
}

impl CommittedConversationView<'_> {
    pub(crate) fn checkpoint(&self) -> &ConversationCheckpoint;
    pub(crate) fn messages(&self) -> &[MessageRecord];
    pub(crate) fn transcript_fingerprint(&self) -> &TranscriptFingerprint;
}
```

CommittedConversationView 只是已验证 State 的只读借用，没有 public constructor；delta 必须先成功 apply并校验 fingerprint，不能直接变成 view。

唯一构造来源：

```text
SessionStorage replay
或
successful CommittedSessionBatch.projections.conversation apply
```

```rust
pub(crate) struct CommittedConversationDelta {
    base: ConversationCheckpoint,
    next: ConversationCheckpoint,
    change: ConversationChange,
}

pub(crate) enum ConversationChange {
    AdvanceOnly,
    Append(Arc<[MessageRecord]>),
    Replace {
        messages: Arc<[MessageRecord]>,
    },
}
```

每个成功 batch 都返回 conversation delta：operational/terminal-no-message commit 使用 AdvanceOnly，消息和 transcript fingerprint 不变，但 checkpoint leaf 前进。Genesis checkpoint 使用 `leaf = None` 和 canonical empty-transcript fingerprint。不提供 arbitrary insert/delete/reorder。

apply：

```text
state.checkpoint == delta.base
→ apply change
→ recompute TranscriptFingerprint
→ verify it == delta.next.transcript_fingerprint
→ publish new state

mismatch
→ discard delta
→ replay selected leaf
```

TranscriptFingerprint 只存于 ConversationCheckpoint，避免 state 内重复 authority。

## Model-Visible Projection

conversation projector 只消费：

```text
TurnStarted.initiating UserMessage
ToolRoundPromoted
SteerCommitted UserMessage
CompactionCommitted
TurnTerminal.Completed final AgentMessage
```

明确忽略：

```text
ToolInvocationStarted
InteractionRequested / Resolved
ToolExecutionStarted
ToolOutcomeKnown
ToolInvocationClosed without ToolRoundPromoted
Interrupted / Failed terminal detail
streaming / retry / progress（根本不持久化）
```

因此 durable 不等于 model-visible。

PromptSet 只能接收 `CommittedConversationView`；该 view 不能从裸 `Vec<MessageRecord>` 构造。

## 其他 Projections

同一个 batch log 重建：

```text
Turn/Item projection
Interaction projection
Recovery projection
Branch/checkpoint projection
Conversation projection
```

不创建第二份 authoritative `chat_history.jsonl` 或 `updates.jsonl`。

loaded Session 可以缓存 projections；cache 丢失后从 log replay。

## Branch Tree

每个 batch 的 parent_leaf 形成 tree：

```text
Genesis
└─ A
   └─ B
      ├─ C
      └─ D
         └─ E
```

不建立 Branch entity。

### Current Leaf

current leaf 是 physical log 中最后成功 commit batch 的 resulting leaf。

普通 append：

```text
expected_current = B
parent = B
→ C
```

从历史 checkpoint 创建 branch：

```text
expected_current = C
parent = B
→ D
```

这同时把 current leaf 推进到 D，并保留 C branch。

只查看历史 leaf 是 transient read selection，不持久化 `HeadMoved` record。只有真正 append 新 fact 才改变 durable current leaf。

## Stable Checkpoint

stable checkpoint 是 projection property。用户可 branch/fork 的 baseline checkpoint 必须满足：

```text
genesis
或
selected leaf 上无 Running Turn、Pending Interaction、Started ToolInvocation，
且 model-visible transcript 不包含 partial atomic unit
```

典型值是 Turn terminal batch leaf，或该 leaf 后追加的 validated CompactionCommitted leaf。

不允许 navigation 到 complete ToolRound 中间、Interaction pending point 或 Tool side-effect barrier 后的半成品状态。

内部 repair 可以扫描其他 batch，但不能把 unsafe leaf 作为普通可执行 Session current leaf。

## Fork

MVP 使用 staging deep copy，不建立 cross-session shared ancestry。

```text
validate source stable checkpoint
→ walk selected root-to-leaf batch path
→ create target staging file
→ write target SessionHeader + ForkOrigin
→ copy/rewrite selected batches
→ remap identities and references
→ replay target and compare semantic projection/fingerprint
→ atomic publish target
```

identity 规则：

| Identity | Child |
| --- | --- |
| SessionId | new |
| BatchId / EntryId | remap |
| TurnId / ItemId / RequestId | remap |
| operation/idempotency key | regenerate |
| ToolCallId | preserve exact value |
| definition/content/fingerprint references | preserve exact semantics |
| loaded waiter/task/lease | never copy |

target 按 selected root-to-checkpoint path 顺序重写 batches。每个 target batch 的 expected_current_leaf 必须改为 staging file 中 immediately previous resulting LeafId；parent_leaf remap 为 selected parent（在线性 copied path 中同样是 previous leaf）；随后使用 target-local operation key 和 remapped logical records 重新计算 batch_fingerprint。

remap 必须递归覆盖：

- StoredSessionBatch expected_current_leaf / parent_leaf；
- every StoredRecord.entry_id；
- ToolInvocationClosed.outcome_entry_id；
- ToolRoundPromoted member outcome EntryId；
- CompactionCommitted source checkpoint leaf、summarized-through、retained/protected EntryIds；
- Turn/Item/Interaction record 内全部 child-local semantic IDs；
- child 内部 ConversationCheckpoint leaf。

ForkOrigin.source_checkpoint 是 source provenance，保留 source Session/Leaf identity，不 remap 为 child leaf。target replay 必须验证全部 nested references 和所有 projections/fingerprints，不只比较 model conversation。

只复制 selected path，不复制 sibling branches。

Fork 不是高频写路径；deep copy 比 copy-on-write DAG 更容易验证、独立删除和 repair。未来若出现高频 fork/remote sync，可在不改变 SessionWriter interface 的前提下增加不同 private backend。

## Compaction

Compaction 是 append-only projection overlay：

```text
raw durable records 不删除
CompactionCommitted 改变 selected branch 的 conversation projection
```

replay：

```text
latest applicable compaction summary
+ retained suffix
+ later model-visible records
```

`applicable` 精确定义为：CompactionCommitted 位于 selected root-to-current path；其 source checkpoint leaf 是 selected leaf 的 ancestor；source transcript fingerprint exact match。

要求：

- source checkpoint 和 transcript fingerprint exact match；
- cut 不拆分 atomic batch/model-visible unit；
- protected EntryId 不进入 summary target；
- navigating/forking 到 compaction 前的 leaf自然不应用该 compaction；
- compaction failure 不改变 projection；
- context compaction 不等于 disk vacuum。

physical retention、log rewrite 和 GC 留到真实容量需求出现后设计。

## Replay

```text
read SessionHeader
→ 逐行读取 complete StoredSessionBatch
→ 验证 format/payload/session、non-empty batch 和 compact-line framing
→ 建立并验证 BatchId/EntryId/operation key 唯一 indexes
→ 从 normalized stored logical payload 重算并验证每个 batch_fingerprint
→ 验证每个 expected_current_leaf 等于 physical append 前的 durable current leaf
→ 验证 parent 仅为 None 或 prior batch-result LeafId，且 graph 无 missing/cycle
→ 取 current leaf
→ root-to-current path fold
→ 构建 Turn/Item/Interaction/Recovery projections
→ 应用 applicable compaction
→ 构建 CommittedConversationState
```

MVP 可以全量 replay。只有真实性能数据证明必要时，才增加：

- reverse scan；
- projection snapshot；
- SQLite metadata/search index；
- cold file compression。

## Projection Snapshot

允许 future optional snapshot：

```rust
pub struct ProjectionSnapshot {
    pub at_batch_id: BatchId,
    pub at_leaf: LeafId,
    pub fold_version: u16,
    pub projection_fingerprint: ProjectionFingerprint,
    // rebuildable data
}
```

规则：

- snapshot 不是 durable truth；
- temp file + atomic rename；
- watermark/fingerprint mismatch 时删除并 replay；
- snapshot write failure 不改变 commit result；
- baseline 可以完全不实现 snapshot。

## Session Catalog / Index

Runtime-global session list/search 可以使用 JSON/SQLite index，但它是 rebuildable：

```text
SessionHeader / lifecycle entity store
→ rebuild catalog
```

index stale、missing 或 corrupt 不能改变 ledger facts。不要把 Codex 式 metadata SQLite 提升为 conversation truth。

## Physical Append 与 Durability

baseline commit procedure：

```text
记录 file old_len
→ serialize complete batch bytes + newline
→ single writer append
→ flush to OS
→ success receipt
```

write failure：

- 能证明未写入：truncate 到 old_len，返回 NotCommitted；
- 无法证明：poison writer，返回 OutcomeUnknown；
- reopen 后按 operation_key replay/lookup；
- 未确认前不能使用新 key重复 logical operation；
- lookup 证明原 ToolOutcomeKnown 已 committed 时使用原 receipt/record；证明未 committed 且 exact result 仍在内存时，只重试 durable result write；只有 executor/side-effect 本身 outcome unknown，或 crash 后 exact result 不可恢复，才关闭为 Abandoned。

当前 contract 与 ADR 0019 一致：

- 保证 process-crash 后只恢复 complete newline-terminated batch；
- 不承诺 power-loss durability；
- 不承诺 Tool side effect 与 ledger exactly-once transaction。

未来若需要 power-loss durability，private implementation 可以增加 `sync_data()`；不增加 per-call durability mode或改变领域接口。

## Batch Size

一个 batch line 可能包含完整 ToolRound。baseline：

- inline structured text/result；
- attachments/images 使用已有 typed reference，而非重复 base64；
- writer 配置一个明确 `max_batch_bytes`；
- 超限在 append 前返回 BatchTooLarge；
- 不自动拆分 atomic ToolRound；
- large blob/content-addressed store 只有真实需求出现后再增加。

## Partial Tail

tail handling 分离 read-only 与 write/recovery path：

```text
read-only replay_at
→ 只读取最后一个 complete newline prefix
→ 发现 partial tail 时不修改文件，可返回 TailIncomplete diagnostics 或 retry

open writable SessionWriter / recovery
→ 获取 exclusive per-session write lease
→ 证明没有 live writer
→ 保存 diagnostics
→ truncate 到最后一个 complete newline
→ replay complete prefix
```

即使尾部 bytes 可以解析成 JSON，只要没有 terminating newline，仍视为 uncommitted tail。read-only replay 永远不能截断可能仍由 live writer 追加的文件。

append 前 writer 必须确保 file 位于最后一个 complete newline boundary；不能像简单“补换行”那样把 torn JSON 变成中间 corrupt record。

## Corruption

以下是 fatal corruption：

- newline-terminated JSON 无法解析；
- unsupported required format/payload version；
- SessionId/header mismatch；
- duplicate BatchId/EntryId；
- duplicate stored operation key（无论 fingerprint 是否相同）；same-key idempotency 只能返回原 batch，不能在 log 中出现第二次；
- recomputed batch_fingerprint mismatch；
- missing/cyclic parent leaf；
- dangling record reference；
- invalid Turn/Item/Interaction transition；
- Interaction family或 resolution key conflict；
- ToolCallId/outcome mismatch；
- ToolRoundPromoted incomplete/misordered；
- compaction checkpoint/fingerprint mismatch。

处理：

```text
read-write open fail closed
→ 不继续 append
→ 返回 structured corruption report
```

不能：

- 静默 skip complete bad line；
- 自动插入 synthetic ToolResult；
- 自动回退到早期 compaction；
- 把 corruption 伪装成 Interrupted Turn。

显式 repair baseline：导出 longest verified prefix，创建新的 repaired Session；不原地猜测修改 source file。

## Crash Recovery

physical replay valid 后，如果 current path 存在 nonterminal Turn：

```text
构造 deterministic recovery operation key
→ one Recovery batch：
     Pending Interaction → Resolved(Cancelled HostRestart/Recovery)
     Started invocation with exact ToolOutcomeKnown → Closed(Completed existing result)
     remaining Started invocation → Closed(Abandoned)
     Turn → Interrupted(HostRestart/RecoveryContextUnavailable)
→ commit
→ replay/apply receipt
```

recovery 不补做缺失 ToolRoundPromoted；exact result 可以关闭 Item，但不会被提升为 model-visible ToolResult。

不恢复：

- provider stream；
- AgentLoop state；
- approval/question waiter；
- Tool task；
- streaming delta；
- queued FollowUp/Steer draft。

## Commit-Before-Publish

```text
construct batch
→ commit
→ apply required projections
→ publish domain event / request notification
```

observer failure不能回滚 batch。

Interaction 特别要求：

```text
InteractionRequested commit
→ request notification

InteractionResolved commit
→ wake waiter / side-effect permission
```

ToolRound 特别要求：

```text
ToolRoundPromoted commit + apply conversation delta
→ feed committed delta to AgentLoop
→ next model call
```

## Events

Session event stream 不是 event store。

- durable facts 由 StoredRecord 表达；
- event 在 commit + projection apply 后发布；
- replay 不要求重放所有历史 observer events；
- streaming/progress event 可丢失；
- subscriber reconnect 通过 snapshot/query 获得 current projection。

不把 transport event payload 直接序列化为 StoredRecord。

## Error 分类

```text
SessionNotFound
WriterBusy
WriterClosed
WriterUnavailable
StaleLeaf
InvalidParentLeaf
UnsafeCheckpoint
OperationConflict
InvalidBatch
BatchTooLarge
NotCommitted
OutcomeUnknown
UnsupportedVersion
CorruptHeader
CorruptBatch
CorruptGraph
CorruptProjection
ForkSourceUnavailable
ForkPublicationFailed
```

公开 error enum 最后在 Runtime interface 阶段冻结。

## 被否决的方案

### 每条 Message 一行

否决原因：ToolCall、ToolResult、final message 和 terminal fact 无法 all-or-none，crash 会暴露 partial protocol history。

### chat_history + operational updates 双日志

否决原因：两个文件可能不同步，必须额外定义谁是 truth、如何 snapshot 和如何 repair。单 batch log 已能投影两者。

### SQLite/WAL 作为 baseline truth

优点是 transaction、index 和 query 强；但当前本地单 writer Runtime 不需要 normalized schema、migration、WAL checkpoint 和 per-session database lifecycle。保留 SessionStorage interface，使未来 adapter 仍可替换。

### Content-addressed commit DAG

优点是 cheap fork 和 structural sharing；但需要 canonical hashing、mutable head CAS、GC、reachability、retention lease 和 shared-prefix purge语义，当前过度设计。

### Cross-session copy-on-write fork

否决原因：source purge、shared corruption、encryption/key rotation 和 GC ownership复杂。MVP fork deep copy selected path。

### Silent corruption skip / synthetic repair

否决原因：跳过 approval、ToolResult 或 terminal record可能构造从未发生的模型历史。MiniCore fail closed并显式 repair。

### Generic storage plugin interface

否决原因：当前只有一个真实 backend。先设计深 SessionStorage module；未来出现第二实现再提取 seam。

## 基础不变量

- SessionStorage 是 Session ledger 唯一 durable truth；
- 每个 Session 同时最多一个 writer；
- runtime mutation 只通过 SessionWriter::commit；
- SessionHeader create/fork 不形成第二 runtime writer；
- 一个 JSONL line 是一个完整 StoredSessionBatch；
- incomplete tail batch 不可见；
- complete invalid line 不静默跳过；
- writer 分配 BatchId/EntryId；
- operation key + fingerprint 保证 idempotency；
- parent_leaf 形成 branch tree；
- current leaf 是最后成功 commit batch leaf；
- Branch 不是 entity；
- durable 不等于 model-visible；
- operational Tool/Interaction facts只推进 ledger checkpoint，不进入 model-visible transcript；
- complete ToolRound promotion 与 Item Completed同 batch；
- CommittedConversationState 只来自 replay 或 trusted commit delta；
- Prompt 只消费 CommittedConversationView；
- hot projection mismatch 时 reload；
- compaction append overlay，不改写 raw history；
- fork 只从 stable checkpoint；
- fork deep copy selected path并 remap target-local identities；
- projection snapshot/index不是 truth；
- recovery 不恢复旧 task/waiter/stream；
- outcome unknown 不重放 Tool或生成 ToolResult；
- terminal/recovery batch关闭 pending/open domain state；
- event 在 commit + projection apply 后发布；
- 当前 contract 只承诺 process-crash durability。

## Test Matrix

至少覆盖：

- create writes one valid immutable SessionHeader；
- concurrent open writer returns WriterBusy；
- one physical compact-JSON line per commit batch，multiline content 必须 escape；
- genesis checkpoint 使用 None leaf；
- writer allocates stable ordered EntryIds/LeafId；
- ordinary append expected/current parent；
- stale expected leaf；
- branch append from stable historical LeafId；
- interior EntryId 不能作为 parent/checkpoint；
- unsafe branch checkpoint rejected；
- same operation key + same fingerprint idempotent receipt，即使 current leaf 已因原 commit 前进；
- same key + different fingerprint conflict；
- log 中 duplicate operation key 即使 fingerprint 相同也 corruption；
- stored batch fingerprint mismatch fails closed；
- NotCommitted safe retry；
- OutcomeUnknown reopen + lookup；
- uncertain ToolOutcomeKnown found committed → preserve exact result；
- uncertain ToolOutcomeKnown proven absent → retry only durable write；
- unresolved storage outcome 不直接派生 Abandoned；
- writer poisoned after uncertain append；
- TurnStart atomic UserMessage + metadata；
- ToolInvocationStarted durable，conversation messages/fingerprint unchanged但 checkpoint leaf AdvanceOnly；
- InteractionRequested commit-before-notify；
- InteractionResolved commit-before-wake；
- ToolExecutionStarted commit-before-side-effect；其 SessionWrite OutcomeUnknown 必须先 lookup resolve，未解析前禁止 side effect；
- pre-execution deny/validation failure 可直接提交 ToolOutcomeKnown，不伪造 ToolExecutionStarted；
- ToolOutcomeKnown durable，messages/fingerprint unchanged但 checkpoint leaf AdvanceOnly；
- complete ToolRound closes all Items and promotes one ordered conversation unit；
- incomplete ToolRound can advance ledger checkpoint but never changes model-visible messages/fingerprint；
- one Abandoned invocation rejects promotion；
- Steer append delta；
- final AgentMessage + TurnTerminal(Completed) atomic；
- terminal batch closes Pending/Started state；
- compaction Replace delta；
- sibling branch compaction 不 applicable；
- terminal 后 compaction leaf 仍可作为 stable checkpoint；
- compaction cut cannot split ToolRound；
- hot delta base mismatch triggers replay；
- replay reconstructs all projections；
- projection snapshot missing/corrupt ignored；
- current leaf follows last committed batch；
- sibling branches retained；
- fork copies only selected path；
- fork rewrites expected_current/parent to target path and recomputes fingerprint；
- fork remaps nested Batch/Entry/Leaf/Turn/Item/Request IDs and references；
- fork preserves ToolCallId/content semantics；
- staging fork crash does not publish child；
- file with complete newline tail loads；
- exclusive writable recovery truncates incomplete last line；
- concurrent/read-only replay does not truncate partial live tail；
- parseable no-newline tail仍 discarded；
- interior malformed line fails closed；
- semantic invalid record fails closed；
- explicit repair exports verified prefix；
- recovery exact result closes Item without promotion；
- recovery unknown outcome Abandoned；
- recovery closes pending Interaction and interrupts Turn once；
- streaming/progress never written；
- max_batch_bytes rejects before append；
- no fsync/power-loss promise in baseline contract。

## 后续问题

1. Session execution 如何组织 batch constructors、commit gate 和 ToolTurnPort implementation。
2. CompactionCommitted summary/cut/retained fields 的最终类型。
3. Runtime query/snapshot 如何分页 Turn/Item/Interaction projection。
4. large Tool output 达到何种数据后引入 blob/content reference store。
5. projection snapshot 触发阈值是否需要实现。
6. Runtime-global Session catalog 使用 JSON、SQLite 还是现有 entity store。
7. physical retention/vacuum 和 purge reachability。
8. future remote/multi-process backend 是否需要提取 storage adapter seam。

## 设计进度

- [x] 选择 per-session append-only JSONL batch log。
- [x] 固定 SessionWriter::commit 唯一 runtime write seam。
- [x] 固定 SessionHeader create/fork exception。
- [x] 定义 StoredSessionBatch、StoredRecord、EntryId、LeafId 和 BatchId。
- [x] 定义 operation key、fingerprint 和 commit receipt。
- [x] 定义 operational facts 与 conversation promotion facts。
- [x] 定义 atomic commit units。
- [x] 定义 genesis-capable CommittedConversationState/Checkpoint、always-returned Delta 和 trusted apply。
- [x] 定义 model-visible projector。
- [x] 定义 parent_leaf branch tree 和 stable checkpoint。
- [x] 定义 fork staging deep copy 和 identity remap。
- [x] 定义 append-only compaction overlay。
- [x] 定义 partial-tail、corruption 和 explicit repair。
- [x] 定义 conservative recovery。
- [x] 拒绝 dual log、baseline SQLite、content DAG 和 generic plugin interface。
- [ ] 完成 Session execution owner 和 batch construction flow。
- [ ] 完成 Compaction module。
- [ ] 完成 Runtime public query/event projection。
