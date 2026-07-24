# Conversation 与 SessionStorage 架构设计

状态：目标架构、ForkAnchor和公开event映射已确定；生产实现待后续完成
日期：2026-07-16

## 目的

本文定义 MiniCore conversation 与 SessionStorage 的 durable ownership、逐 entry JSONL、唯一写入 seam、projection、branch/fork、compaction、reload、corruption 和 recovery 语义。

本文重点解决：

- SessionStorage 如何成为 Session conversation/execution ledger 的唯一 durable truth；
- 一个物理 JSONL entry 如何同时保持可检查、可分支和可恢复；
- User、Assistant、Tool message、Reasoning、usage 和 durable event 如何分类；
- 哪些已持久化内容可以进入模型 conversation；
- ToolCall、ToolResult、Interaction和ToolExecutionStarted前置记录如何逐entry持久化；
- complete ToolRound 如何在没有物理 batch 的前提下 all-or-none 进入模型 conversation；
- entry-level operation idempotency 和 append acknowledgement unknown 如何处理；
- hot projection 如何只应用成功 append receipt；
- branch、fork、compaction、partial tail、corruption 和 process-crash recovery。

本文不提前冻结：

- SessionExecutor、request queue和异步operation的具体实现；
- Runtime command、query、event 和 snapshot payload；
- ModelGateway provider adapter；
- compaction summary prompt 和 token budget；
- Runtime-global Session catalog 的物理数据库；
- power-loss durability、remote replication 或多进程 multi-writer；
- large payload blob store、physical vacuum 和 retention policy；
- SQLite、object store 或 content-addressed backend。

## 同类项目结论

| 项目 | Durable layout | 采用点 | 避免点 |
| --- | --- | --- | --- |
| pi | header + `id/parentId` entry tree；一个 finalized AgentMessage 保存 thinking、text、tool calls、usage；ToolResult 独立 message | 单文件、逐 entry、parent tree、assistant response 聚合 | approval和execution-start required records不durable；ToolCall/ToolResult 直接进入 transcript，partial round 依赖上层修复 |
| Codex | 一行一个 `RolloutItem`；ResponseItem、Reasoning、FunctionCall/Output、TurnContext、Compacted、EventMsg 并列 | typed entry stream、reasoning/context fidelity、turn-boundary fork | ResponseItem 与 EventMsg 内容重复；事件类型过多；ToolCall/Output 物理独立但没有MiniCore ToolRoundCompleted模型可见性规则 |
| Claude Code | 一行一个 uuid/parentUuid record；user/assistant/system/tool_use/tool_result/thinking/usage/file snapshot 等 | 可导航 history、reasoning signature、usage 与 response 同存 | thinking/tool_use 常拆为多 assistant records并重复 usage；执行事件和文件 checkpoint 混入同一历史 |
| Grok Build | `chat_history.jsonl` ConversationItem + `updates.jsonl` update stream + summary/checkpoint sidecar | reasoning replay、prompt-index rewind、durable update ordering | 两个 conversation-like logs、sidecar ownership、重放/修复 surface 较重 |

MiniCore 采用：

```text
pi 的单文件 entry tree 与 assistant response 聚合
+ Codex/Grok 的 reasoning、context 和 provider-continuity fidelity
+ MiniCore的durable Interaction / ToolExecutionStarted前置记录
+ committed-only ToolRoundCompleted模型可见性规则
```

MiniCore 不复制：

- 一行一个业务 batch；
- Codex 的完整 EventMsg rollout；
- Grok 的 chat/update 双 durable log；
- Claude 的 file-history checkpoint；
- pi 的任意 message 直接进入模型 transcript。

## 决策摘要

已经确定：

- SessionStorage 是 Session conversation/execution ledger 的唯一 durable truth；
- 每个 writable Session 同时最多一个 `SessionWriter`；
- 已创建 Session 的全部 ledger mutation 通过 `SessionWriter::append(SessionEntryDraft)`；
- SessionHeader 是第一行，由 create/fork staging 原子写入；
- 后续每一物理行是一条完整 `StoredSessionEntry`；
- 一条 newline-terminated entry 是 process-crash visibility unit；
- entry 使用 `EntryId + parent_id` 形成 immutable history tree；
- 不建立 `BatchId`、`StoredSessionBatch`、batch fingerprint 或 interior-entry 概念；
- operation key 属于单条 entry intent；同 key同 normalized payload 返回原 receipt，同 key不同 payload conflict；
- storage append acknowledgement unknown 必须按 operation key reopen/replay；
- entry 顶层类别固定为 `TurnContext | Message | Event | Compaction`；
- Message 使用 `role = user | assistant | tool`；
- 一个 finalized logical model response 保存为一个 assistant message entry；
- assistant `content[]` 按原始顺序保存 reasoning、text 和 tool_call；
- usage、model、response ID、finish reason 和 retry summary随 assistant entry 保存；
- 每个 ToolCall 的 truthful ToolResult 保存为独立 `role = tool` message entry；
- initiating user message append 是 Turn 开始线性化点；
- final assistant message append 是 Completed Turn 结束线性化点；
- Interrupted/Failed Turn 由 durable event terminalize；
- assistant tool-call response 和 tool messages在 `tool_round_completed` event前都不进入模型 conversation；
- `tool_round_completed`引用一个assistant entry及其全部ordered tool entries，并作为complete ToolRound进入conversation projection的required record；
- Interaction request/resolution、Tool execution-start 和 Tool abandoned使用 durable event；
- Runtime observer event 从 committed entry receipt派生，不原样写入 Session ledger；
- streaming delta、Tool progress、provider retry attempt 和 transient phase不持久化；
- `CommittedConversationState` 只能由 replay或成功应用 trusted entry delta推进；
- compaction 是独立 entry并执行 conversation Replace；
- fork deep-copy selected parent path并 remap target-local identities；
- 只忽略/截断最后一个未换行partial line；newline-terminated invalid entry是 corruption；
- recovery不生成 synthetic ToolResult，不自动重放 outcome-unknown Tool；
- projection snapshot、session index和search database只是 rebuildable cache。

## Ownership

```text
MiniCoreRuntime
└─ SessionStorage
   ├─ create/open/fork
   ├─ per-session SessionWriter
   │  └─ append(SessionEntryDraft)
   ├─ replay / projection rebuild
   └─ private JSONL implementation
```

SessionStorage拥有：

- Session ledger file；
- EntryId allocation；
- operation key index；
- current entry和parent graph reconstruction；
- entry validation和cross-entry reference validation；
- durable append；
- replay/projector；
- partial-tail handling；
- explicit corruption diagnostics；
- fork staging copy和identity remap。

SessionStorage不拥有：

- Agent/Session execution scheduling；
- Prompt assembly；
- Tool permission或execution；
- Workspace authorization；
- provider stream / AgentLoop；
- approval/question waiter；
- Runtime public transport；
- Workspace file rollback。

## 最小接口

当前只有一个真实 backend，因此不建立 `dyn SessionStorageAdapter` hierarchy。

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
    pub async fn append(
        &mut self,
        draft: SessionEntryDraft,
    ) -> Result<CommittedSessionEntry, SessionWriteError>;
}
```

不提供：

```text
append_raw_json
append_arbitrary_message
set_current_entry_without_append
write_projection
replace_history
save_runtime_state
generic transaction callback
```

业务 helper 可以存在：

```text
append_turn_context(...)
append_user_message(...)
append_assistant_message(...)
append_tool_message(...)
append_interaction_event(...)
append_tool_execution_started(...)
append_tool_round_completed(...)
append_turn_interrupted(...)
append_compaction(...)
```

但它们只能构造 validated `SessionEntryDraft`并调用同一个 writer。

## 物理文件

MVP layout：

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
    pub source_transcript_fingerprint: TranscriptFingerprint,
}
```

Header immutable。Session name、lifecycle和current definition pointer等mutable entity metadata不通过重写Header更新。

by-entry layout是首个正式Session ledger `format_version = 1`。旧`StoredSessionBatch`只存在于被supersede的设计文档，没有仓库内已发布production wire data，因此MVP不实现legacy batch reader/migrator。未来只有在真实format v2出现时才定义version migration。

每条 entry 使用 compact single-line JSON serialization：禁止pretty print，字符串中的换行/control characters必须JSON escape。

示意：

```jsonl
{"type":"session","formatVersion":1,"sessionId":"s1","createdAt":"2026-07-16T10:00:00Z"}
{"id":"e1","parentId":null,"timestamp":"2026-07-16T10:00:01Z","operationKey":"turn:t1:context","type":"turn_context","turnId":"t1","executionFingerprint":"ctx1"}
{"id":"e2","parentId":"e1","timestamp":"2026-07-16T10:00:02Z","operationKey":"turn:t1:user","type":"message","role":"user","turnId":"t1","itemId":"i1","source":"input","contextEntryId":"e1","content":[{"type":"text","text":"读取 README"}]}
```

## Entry Envelope

```rust
pub struct StoredSessionEntry {
    pub format_version: u16,
    pub entry_id: EntryId,
    pub parent_id: Option<EntryId>,
    pub timestamp: Timestamp,
    pub operation_key: IdempotencyKey,
    pub body: StoredEntryBody,
}

pub enum StoredEntryBody {
    TurnContext(StoredTurnContext),
    Message(StoredMessage),
    Event(StoredDurableEvent),
    Compaction(StoredCompaction),
}
```

基础规则：

- `entry_id` 由 writer分配，caller不能预分配；
- `parent_id` 是该 entry 的logical parent；
- ordinary append的parent是writer current entry；
- history branch append可以使用已验证历史entry作为parent；
- `operation_key` 标识一条caller intent/retry；
- operation key lookup发生在current-entry conflict检查之前；
- normalized payload fingerprint由writer/index内部计算，不要求序列化独立hash字段；
- 同key同normalized payload返回原 `CommittedSessionEntry`；
- 同key不同payload返回 `OperationConflict`；
- JSON object field order不承担语义；
- EntryId、ItemId、ToolCallId、TurnId、RequestId不能混用。

`SessionEntryDraft` 至少包含：

```rust
pub struct SessionEntryDraft {
    expected_current_entry: Option<EntryId>,
    parent_entry: Option<EntryId>,
    operation_key: IdempotencyKey,
    body: PendingEntryBody,
}
```

`expected_current_entry`是调用方观察到的current head；`parent_entry`是新entry parent。ordinary append两者相同。fork staging和显式history branch可以不同，但必须通过writer validation。

## TurnContext Entry

```rust
pub struct StoredTurnContext {
    pub turn_id: TurnId,
    pub session_definition: StoredSessionDefinitionRef,
    pub agent: AgentRevisionRef,
    pub model: TurnModelRef,
    pub workspace: WorkspaceSnapshotRef,
    pub prompt_fingerprint: PromptFingerprint,
    pub tool_fingerprint: ToolSetFingerprint,
    pub skill_fingerprint: SkillCatalogFingerprint,
    pub execution_fingerprint: ExecutionContextFingerprint,
    pub diagnostics: Arc<[StoredContextDiagnostic]>,
}

pub struct StoredSessionDefinitionRef {
    pub session_id: SessionId,
    pub revision: SessionDefinitionRevision,
    pub content_fingerprint: ContentHash,
}

pub struct TurnModelRef {
    pub selection: ModelSelection,
    pub definition_version: ModelDefinitionVersion,
    pub turn_model_fingerprint: TurnModelFingerprint,
}
```

`StoredSessionDefinitionRef`是历史exact reference，不是“读取target Session current head”的指令。`TurnModelRef`保存stable selection、exact model definition version和覆盖effective capability/generation policy的TurnModelFingerprint；它不保存endpoint、auth binding或credential。被fork的历史TurnContext保留source-scoped exact reference；child的`SessionDefinitionRevision(1)`只治理future Turn。retention/purge必须保留仍被历史entry引用的immutable definition content。

Context entry在initiating UserMessage前append，由UserMessage通过`context_entry_id`引用。

Context append成功不创建领域Turn，也不允许调用模型或执行Tool。若进程在Context entry后、UserMessage前崩溃，该entry是安全的orphan preparation fact；replay不会把它投影为Running Turn或conversation message。

Context不保存：

- provider credentials；
-随机lease token；
- approval waiter；
- cancellation token；
- mutable cache state；
- executor内存地址。

## Message Entry

```rust
pub enum StoredMessage {
    User(StoredUserMessage),
    Assistant(StoredAssistantMessage),
    Tool(StoredToolMessage),
}
```

JSON使用常规role：

```text
role = user | assistant | tool
```

### User Message

```rust
pub struct StoredUserMessage {
    pub turn_id: TurnId,
    pub item_id: ItemId,
    pub source: StoredUserMessageSource,
    pub context_entry_id: Option<EntryId>,
    pub content: Arc<[UserContent]>,
}

pub enum StoredUserMessageSource {
    Input,
    Steer,
}
```

规则：

- `source = Input` 必须引用同一TurnId的TurnContext entry；
- Input entry append是领域Turn开始线性化点；
- 同时只能有一个Running Turn；
- `source = Steer` 必须绑定expected Running Turn，不重新捕获Context；
- Steer entry append后才影响下一次逻辑模型调用；
- FollowUp不保存为Steer，它在下一Turn写新的Input message；
- Interaction resolution不是User message。

Input/Steer message entry本身就是canonical content和durable lifecycle fact，不再重复写`TurnStarted`或`SteerCommitted` event。

### Assistant Message

一个finalized logical model response保存为一个assistant entry。stream delta和partial draft不持久化。

```rust
pub struct StoredAssistantMessage {
    pub turn_id: TurnId,
    pub phase: StoredAssistantPhase,
    pub model: TurnModelRef,
    pub response_id: Option<String>,
    pub content: Arc<[AssistantContent]>,
    pub usage: Option<ModelUsage>,
    pub finish_reason: ModelFinishReason,
    pub provider_metadata: StoredProviderResponseMetadata,
    pub retry_count: u32,
    pub assembled_context_fingerprint: AssembledModelContextFingerprint,
}

pub struct StoredProviderResponseMetadata {
    pub provider_request_id: Option<String>,
    pub raw_finish_code: Option<String>,
    pub service_tier: Option<String>,
}

pub enum StoredAssistantPhase {
    Intermediate,
    Final {
        completion: TurnCompletion,
    },
}
```

```rust
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

规则：

- content order是模型最终输出的canonical顺序；
- ToolCall name/arguments/index只在assistant entry保存一次；
- 每个ToolCall block创建一个ToolInvocation Item的Started projection；
- 同一assistant entry内ToolCallId和ItemId必须唯一；
- `phase = Intermediate`必须包含至少一个ToolCall；text/reasoning-only稳定响应若结束Turn必须使用`Final`，否则只作为transient draft/observer output；
- Intermediate assistant entry durable/UI-visible，但在完整ToolRound前不进入模型conversation；
- `phase = Final`不能包含未满足ToolCall；
- Final assistant entry append是Completed Turn的结束线性化点；
- Final append前必须验证Turn仍Running、没有Pending Interaction或Started ToolInvocation、没有尚未被`tool_round_completed`引用的Intermediate assistant entry，且cancel/Steer没有赢得仲裁；
- usage属于该逻辑模型响应，不另写TokenCount event；
- retry_count只表示SessionExecutor对同一logical call执行的logical retry数量，不包含ModelGateway transparent attempt；
- provider_metadata只保存allowlisted bounded code/request ID，不保存raw response、headers、endpoint或payload；
- Session total usage由assistant entries和带model_call的Compaction entries重建为projection/cache。

### Reasoning

```rust
pub struct StoredReasoning {
    pub item_id: ItemId,
    pub text: Option<String>,
    pub summary: Option<String>,
    pub encrypted: Option<String>,
    pub signature: Option<String>,
    pub provider_item_id: Option<String>,
}
```

只保存provider实际返回的finalized/replayable reasoning artifact：

- 可显示thinking text或summary；
- provider opaque encrypted content；
- signature和provider item ID。

不保存：

- 未暴露的hidden chain-of-thought；
- reasoning streaming delta；
- partial draft；
- caller自行推断的reasoning文本。

Prompt/ModelGateway按provider capability决定后续调用是否回放text、summary、encrypted artifact或完全忽略；storage不把reasoning强制转成普通assistant text。

### Usage

```rust
pub struct ModelUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub provider_total_tokens: Option<u64>,
    pub reported_cost: Option<Money>,
}
```

provider response是瞬时值；若不持久化，process restart后会失去historical usage、cost和cache统计。因此AgentRun usage随assistant response保存，CompactionSummary usage随StoredCompaction.model_call保存。

provider未返回的字段保持`None`，不能以估算冒充provider truth。provider_total_tokens只保存provider报告值；本地字段求和不能伪装成provider total。本地context estimate和catalog price计算属于rebuildable projection，不写入usage事实；reported_cost只保存provider明确返回的billed cost。

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
    Executed {
        execution_started_entry_id: EntryId,
    },
}
```

Tool message是exact truthful ToolResult的canonical durable source。

规则：

- 必须匹配同一parent path上某个assistant ToolCall block；
- `PreExecution`用于validation/policy/approval deny/unavailable等没有side effect的结果；
- `Executed`必须引用同一ItemId/ToolCallId的`tool_execution_started` event；
- append成功后ToolInvocation Item进入Completed operational state；
- Tool message durable不等于model-visible；
- 只有`tool_round_completed`成功后，该Tool message才进入conversation；
- outcome unknown不生成Tool message；
- 非幂等Tool不能因为Tool message append acknowledgement unknown而重新执行。

## Durable Event Entry

```rust
pub enum StoredDurableEvent {
    InteractionRequested(StoredInteractionRequested),
    InteractionResolved(StoredInteractionResolved),
    ToolExecutionStarted(StoredToolExecutionStarted),
    ToolAbandoned(StoredToolAbandoned),
    ToolRoundCompleted(StoredToolRoundCompleted),
    TurnInterrupted(StoredTurnInterrupted),
    TurnFailed(StoredTurnFailed),
}
```

这些是恢复、权限、副作用和conversation projection所需的durable event，不是Runtime公开EventMsg的序列化副本。

Runtime public event必须从成功append并apply的entry receipt派生。Session ledger不保存：

```text
stream delta
Tool progress
Sampling / ExecutingTools / WaitingApproval phase
每次provider retry
queue update
heartbeat
普通observer notification
```

### InteractionRequested / Resolved

```text
InteractionRequested
→ TurnId + ItemId + RequestId + typed request + expires_at

InteractionResolved
→ TurnId + ItemId + RequestId + typed resolution + resolution_key
```

顺序：

```text
interaction_requested append + apply
→ notify host

interaction_resolved append + apply
→ wake waiter / allow protected continuation
```

deadline和resolution必须由单一SessionExecutor线性化。late resolution不得覆盖已committed timeout/cancel resolution。

### ToolExecutionStarted

```rust
pub struct StoredToolExecutionStarted {
    pub turn_id: TurnId,
    pub item_id: ItemId,
    pub tool_call_id: ToolCallId,
    pub resolved_tool_name: ToolName,
    pub invocation_fingerprint: ContentHash,
    pub requirements_fingerprint: ContentHash,
    pub authorization_fingerprint: ContentHash,
    pub started_at: Timestamp,
}
```

必须在可能发生外部副作用前append并apply：

```text
tool_execution_started append + apply
→ side effect
```

该entry证明准备执行的frozen intent，但baseline recovery仍不自动重放。

### ToolAbandoned

```rust
pub struct StoredToolAbandoned {
    pub turn_id: TurnId,
    pub item_id: ItemId,
    pub tool_call_id: ToolCallId,
    pub reason: ToolAbandonReason,
}
```

只有没有truthful ToolResult的终止路径使用Abandoned，例如：

- executor/side-effect outcome unknown；
- host restart后exact result不可恢复；
- cancellation在允许无结果关闭的pre-execution阶段获胜。

Abandoned没有Tool message，也不能进入complete ToolRound conversation。

### ToolRoundCompleted

```rust
pub struct StoredToolRoundCompleted {
    pub turn_id: TurnId,
    pub assistant_entry_id: EntryId,
    pub tool_entry_ids: Arc<[EntryId]>,
}
```

`tool_round_completed`是完整ToolRound进入conversation projection的required record，不是ToolRound entity。

append前writer必须验证：

- assistant entry位于同一Session selected parent path；
- assistant phase为Intermediate；
- assistant至少包含一个ToolCall；
- 所有ToolCall属于同一Turn；
- 每个ToolCallId/ItemId exactly once；
- tool entries与ToolCall集合exact match；
- 每个tool entry位于同一path且早于completion event；
- 每个tool entry的source reference合法；
- tool entries按assistant ToolCall index规范化排序；
- 不含Abandoned或outcome-unknown invocation；
- 同一assistant entry未被另一个completion event重复完成。

append成功后：

- 所有关联ToolInvocation已是Completed operational state；
- conversation projector一次性追加assistant content和ordered tool messages；
- transcript fingerprint只在此entry改变；
- 下一次实际模型调用才可开始。

若crash发生在部分/全部tool message已append但completion event尚未append：

- exact ToolResult仍是durable truth；
- ToolRound不进入模型conversation；
- baseline restart不自动补写completion event；
- recovery保留Completed operational Items，Abandon剩余Started Items，并Interrupted当前Turn。

### TurnInterrupted / TurnFailed

```text
TurnInterrupted → TurnId + typed reason + completed_at
TurnFailed      → TurnId + typed failure + completed_at
```

它们用于没有final assistant message的terminal path。

append前必须验证：

- 所有Pending Interaction已通过resolved event关闭；
- 所有Started ToolInvocation已有Tool message或ToolAbandoned event；
- 不再允许新Model/Tool work；
- 同一Turn没有其他terminal fact。

Interrupted/Failed event不向model conversation追加synthetic assistant message。

## Compaction Entry

`ConversationBoundary`引用一个完整model-visible stable unit：

```rust
pub struct ConversationBoundary {
    pub unit_first_entry_id: EntryId,
    pub unit_last_entry_id: EntryId,
    pub unit_fingerprint: StableConversationUnitFingerprint,
}

pub struct StableConversationUnitFingerprint(pub ContentHash);
```

`summarized_through`表示最后一个被摘要unit，inclusive；`retained_from`表示第一个原样保留unit，inclusive。writer通过source projection证明两者在ordered stable-unit sequence中相邻。`unit_fingerprint`覆盖unit kind、ordered model-visible content和backing reference shape，但不覆盖storage-local EntryId，因此fork remap first/last EntryId后fingerprint保持不变。

```rust
pub struct StoredCompaction {
    pub turn_id: Option<TurnId>,
    pub source: ConversationCheckpoint,
    pub summarized_through: ConversationBoundary,
    pub summary: CompactionSummary,
    pub retained_from: ConversationBoundary,
    pub protected_entries: Arc<[EntryId]>,
    pub model_call: Option<StoredCompactionModelCall>,
}

pub struct StoredCompactionModelCall {
    pub model: TurnModelRef,
    pub response_id: Option<String>,
    pub usage: Option<ModelUsage>,
    pub finish_reason: ModelFinishReason,
    pub provider_metadata: StoredProviderResponseMetadata,
    pub logical_retry_count: u32,
    pub assembled_context_fingerprint: AssembledModelContextFingerprint,
}
```

Compaction entry本身触发conversation Replace，不另写`compaction_completed` durable event。

规则：

- source checkpoint和TranscriptFingerprint必须exact match；
- summarized range必须是连续prefix，retained range必须是连续suffix，两者在stable boundary相邻；
- cut不能拆分Input/Steer UserMessage、complete ToolRound、final assistant或existing Compaction summary；
- active-Turn automatic Compaction必须保护initiating UserMessage及其后的连续suffix；
- caller不提交raw replacement messages；trusted projector从source、boundaries和summary构造`Replace([summary] + retained suffix)`；
- compaction只追加overlay，不重写旧entries；
- navigating/forking到compaction前的entry自然不应用该compaction；
- 首版automatic Compaction固定`turn_id = Some(active TurnId)`且必须保存SummaryModel `model_call`；`None`只为未来maintenance/deterministic method预留；
- model_call usage与logical_retry_count遵守和assistant response相同的provider-truth、redaction和retry语义；
- physical retention/vacuum与conversation compaction分离。

完整planning、protection和failure规则见[Compaction架构设计](compaction.md)。

## SessionWriter Append Algorithm

```text
SessionWriter.append(draft)
→ 检查writer未poisoned/closed
→ 先lookup operation_key
   ├─ same key + same normalized payload：返回原receipt
   └─ same key + different payload：OperationConflict
→ 检查expected_current_entry
→ 验证parent_entry存在且可作为本次append parent
→ 分配EntryId和timestamp
→ 验证entry body与所有cross-entry references
→ 计算trusted projection deltas
→ serialize one StoredSessionEntry + '\n'
→ append + flush process-visible bytes
→ 更新writer current entry / indexes
→ 返回CommittedSessionEntry
```

append一旦进入physical write，不接受Turn cancellation。

内部可以使用buffered writer或合并OS write以降低syscall，但每条entry仍有独立identity、operation key、validation和receipt；不得重新形成caller-visible业务batch协议。

## CommittedSessionEntry

```rust
pub(crate) struct CommittedSessionEntry {
    entry: Arc<StoredSessionEntry>,
    previous_current_entry: Option<EntryId>,
    current_entry: EntryId,
    projections: CommittedProjectionDeltaSet,
}

pub(crate) struct CommittedProjectionDeltaSet {
    conversation: CommittedConversationDelta,
    // private typed Turn/Item/Interaction/Recovery/tree deltas
}
```

receipt字段不直接公开；只提供diagnostics/read-only accessors和storage-owned `apply_committed`。

全部projection delta由SessionStorage trusted projectors生成，不接受caller-provided delta。Session execution必须先apply required projections，随后才能：

- 发布Runtime event；
- notify Interaction request；
- wake Interaction waiter；
- 开始Tool side effect；
- 开始下一次模型调用。

apply失败不回滚已append entry；Session execution丢弃hot projections并从durable current entry replay。

## Append Error

```rust
pub enum SessionWriteError {
    StaleCurrentEntry,
    InvalidParent,
    OperationConflict,
    InvalidEntry,
    EntryTooLarge,
    NotCommitted(StorageError),
    OutcomeUnknown {
        operation_key: IdempotencyKey,
    },
    WriterUnavailable(StorageError),
}
```

语义：

- `NotCommitted`：失败发生在physical write前，已证明entry不存在，可以安全retry同一draft；
- physical write开始后的write/flush错误返回`OutcomeUnknown`并poison当前writer；
- `OutcomeUnknown`：必须取得exclusive lease后reopen，处理partial tail并replay/按operation key lookup；解析完成前不能继续使用旧writer；
- `OutcomeUnknown`表示storage acknowledgement不确定，不等于Tool side-effect outcome unknown；
- `WriterUnavailable`：writer已poisoned/closed，不能继续append；
- same key同payload retry返回原receipt，不产生第二条log entry；
- log中出现duplicate operation key属于corruption。

## Conversation Projection

```rust
pub(crate) struct CommittedConversationState {
    checkpoint: ConversationCheckpoint,
    messages: Arc<[MessageRecord]>,
}

pub(crate) struct ConversationCheckpoint {
    entry_id: Option<EntryId>,
    transcript_fingerprint: TranscriptFingerprint,
}

pub(crate) struct CommittedConversationView<'a> {
    checkpoint: &'a ConversationCheckpoint,
    messages: &'a [MessageRecord],
}
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

每个成功append都推进checkpoint entry_id：

- operational/context/event entry通常是`AdvanceOnly`；
- Input/Steer user和Final assistant是`Append`；
- `tool_round_completed`是`Append`；
- Compaction是`Replace`。

`AdvanceOnly`不改变messages或TranscriptFingerprint，但推进ledger checkpoint。

唯一构造来源：

```text
SessionStorage replay
或
successful CommittedSessionEntry delta apply
```

`CommittedConversationView`没有public constructor，不能从draft、stream buffer或裸message vector构造。

## Model-Visible Rules

conversation projector只消费：

```text
Message::User(source = Input)
Message::User(source = Steer)
Event::ToolRoundCompleted
Message::Assistant(phase = Final)
Compaction
```

明确不直接消费：

```text
TurnContext
Message::Assistant(phase = Intermediate)
Message::Tool
InteractionRequested / Resolved
ToolExecutionStarted
ToolAbandoned
TurnInterrupted / TurnFailed
streaming / retry / progress
```

`ToolRoundCompleted`投影：

```text
assistant_entry.content
→ provider-neutral assistant/reasoning/tool-call sequence
ordered tool_entry_ids
→ matching tool messages
```

任何missing/mismatched reference都fail closed，不能跳过坏member后继续构造模型输入。

## 其他 Projections

同一个entry tree重建：

```text
Turn projection
Item projection
Interaction projection
Recovery projection
Conversation projection
Usage projection
Tree/checkpoint projection
```

不创建第二份authoritative `chat_history.jsonl`、`events.jsonl`或usage ledger。

Runtime public event stream、snapshot和Session index都由entry receipt/replay投影。

## Entry Tree 与 Current Entry

每个entry的`parent_id`形成tree：

```text
Genesis
└─ e1 TurnContext
   └─ e2 User
      └─ e3 Assistant(intermediate)
         ├─ e4 Tool...
         └─ e8 Historical branch...
```

不建立Branch entity。

current entry是physical log中最后成功append entry。ordinary append：

```text
expected_current = e3
parent = e3
→ e4
```

history branch append：

```text
expected_current = e7
parent = e3
→ e8
```

这保留e4...e7旧path，并让e8成为new current entry。

所有entry都可作为storage tree node，但Runtime对外navigation/fork interface可以限制可选择的message anchor。

## Fork

ForkSession创建新SessionId，不恢复source执行上下文。

MVP公开anchor：

```text
Before UserMessage
After UserMessage
Before final AssistantMessage
After final AssistantMessage
```

anchor精确解析：

- `Before UserMessage(Input)`解析为其TurnContext entry的`parent_id`，同时排除Context和Input，避免复制orphan Context；
- `Before UserMessage(Steer)`解析为该UserMessage的`parent_id`；
- `After UserMessage`解析为该UserMessage entry；Input场景自然包含其TurnContext；
- `Before final AssistantMessage`解析为final entry的`parent_id`；
- `After final AssistantMessage`解析为final entry；
- genesis解析为`None`；`ForkOrigin.source_entry_id`保存解析后的path end，而不是picker message的模糊位置。

带ToolCall的intermediate assistant、Tool message、Interaction/event entry不作为公开picker anchor。storage内部仍可按任意EntryId执行只读replay和repair。

fork流程：

```text
resolve requested message anchor
→ flush/open source complete-entry prefix
→ walk selected root-to-entry parent path
→ validate references needed by selected path
→ create target staging file
→ write target SessionHeader with ForkOrigin
→ copy entries in path order
→ remap target-local identities/references
→ if copied prefix contains Running Turn:
     close Pending Interaction
     mark remaining Started ToolInvocation Abandoned
     append TurnInterrupted(HistoricalFork)
→ full replay/validation of target staging
→ atomic publish target
→ publish child Session entity
```

remap：

| Identity | Fork handling |
| --- | --- |
| entry ownership/correlation SessionId | new target value；ForkOrigin与historical exact definition refs保留source identity |
| EntryId / parent_id | remap |
| TurnId / ItemId / RequestId | remap |
| operation key | regenerate target-local key |
| ToolCallId | preserve |
| historical Agent/SessionDefinition/Workspace exact refs | preserve source-scoped exact semantics；不重写为child current revision |
| content/model/reasoning payload | preserve exact semantics, subject to redaction policy |

必须重写所有nested EntryId references：

- User context_entry_id；
- Tool source execution_started_entry_id；
- ToolRoundCompleted assistant/tool entry refs；
- Compaction checkpoint/protected refs，以及`summarized_through`/`retained_from` boundary中的`unit_first_entry_id`和`unit_last_entry_id`；
- Fork provenance保留source identity但不能作为target operational reference。

Fork不复制：

```text
loaded state
TurnExecutionContext object
Workspace authorization lease
provider session
AgentLoop
waiter/task
FollowUp queue
Session-scoped grants
```

child下一Turn重新捕获Context。

## Compaction、Fork 与历史分支

Compaction只改变selected path上的conversation projection：

```text
root → old messages → compaction entry → retained tail
```

fork到compaction前的entry得到旧conversation；fork到compaction后的message anchor应用compaction overlay。

Compaction protection使用EntryId，但必须保护完整model-visible semantic unit：

- initiating user entry；
- ToolRoundCompleted及其referenced assistant/tool entries；
- final assistant entry。

automatic active-Turn Compaction还必须保护initiating UserMessage及其后的连续suffix。不允许cut产生孤立ToolCall、Tool message或从prefix中挖洞。

## Reload

```text
read SessionHeader
→ 逐行读取complete newline-terminated StoredSessionEntry
→ 验证format/session/EntryId/operation key唯一性
→ 建立parent graph
→ 验证parent只引用prior complete entry或None
→ 验证cross-entry references位于同一selected ancestry并且类型匹配
→ fold Turn/Item/Interaction/Conversation/Usage projections
→ 确定physical current entry
→ 检测partial tail、corruption和unfinished Turn
```

Read-only replay：

- 只读取最后一个complete newline prefix；
- 发现live writer partial tail时不截断；
- 可以返回`TailIncomplete` diagnostics/retry。

Writable open/recovery：

- 必须先取得exclusive writer lease；
- 才能truncate最后一个未换行partial tail；
- 随后完整replay并开始recovery。

## Corruption

严格fail closed：

只允许自动忽略/截断：

- 文件最后一个未换行partial JSON fragment。

以下是corruption：

- newline-terminated invalid JSON；
- duplicate EntryId；
- duplicate operation key；
- missing/cyclic/forward parent；
- cross-session/cross-branch illegal reference；
- User input引用错误Context；
- Tool message找不到matching ToolCall；
- Executed Tool message引用错误execution-start；
- ToolRoundCompleted missing/duplicate/misordered calls/results；
- final assistant仍有Pending/Started state；
- terminal Turn后出现属于同一Turn的new work；
- compaction source checkpoint/fingerprint mismatch；
- unknown authoritative core entry variant/version。

未知的authoritative entry不能像普通UI event一样静默skip。未来如果需要可忽略extension diagnostics，应使用独立、明确non-authoritative trace facility，而不是向Session durable enum加入任意`Value`。

## Conservative Recovery

process restart后：

```text
replay complete entries
→ 不恢复provider stream、AgentLoop、waiter或Tool task
→ 若没有Running Turn：Ready/Idle
→ 若存在Running Turn：
     resolve/cancel every Pending Interaction with stable operation key
     preserve every existing Tool message as truthful Completed Item
     append ToolAbandoned for remaining Started invocation
     append TurnInterrupted(HostRestart | RecoveryContextUnavailable)
→ apply each receipt
→ Ready/Idle
```

recovery逐entry执行，不承诺多entry all-or-none。每个recovery operation使用由TurnId/ItemId/RequestId和recovery reason派生的稳定operation key，因此crash后可继续未完成cleanup。

重要规则：

- existing Tool message不因缺少ToolRoundCompleted而丢失或改写；
- baseline不自动补写ToolRoundCompleted；
- outcome-unknown Tool不自动重放；
- 不生成synthetic ToolResult；
- Interrupted/Failed不向conversation添加synthetic assistant message；
- 如果terminal event已存在但仍有Pending Interaction或Started ToolInvocation，属于semantic corruption，而不是继续append掩盖。

## Append/Apply-Before-Visibility / Side Effect / Notify

统一顺序：

```text
SessionWriter.append(draft)
→ resolve OutcomeUnknown if necessary
→ storage-owned apply_committed(receipt)
→ publish/wake/side-effect/model-call
```

具体required ordering：

```text
User input append/apply
→ publish Turn started/User message
→ first model call

InteractionRequested append/apply
→ notify host

InteractionResolved append/apply
→ wake waiter

ToolExecutionStarted append/apply
→ side effect

Tool message append/apply
→ Tool outcome available as durable operational fact

ToolRoundCompleted append/apply
→ complete ToolRound enters conversation
→ next model call

Final assistant append/apply
→ Turn Completed
```

observer failure不能回滚durable entry。

## Performance 与 Size

By-entry会增加line count和append调用次数，但降低单条line复杂度并提高inspect/fork灵活性。

MVP要求：

- writer可以复用open file handle和buffer；
- `append()` success语义仍必须明确，不因buffering返回未写入的false success；
- 不使用每条entry `fsync`宣称power-loss durability；
- process-crash baseline要求newline-terminated complete entry可replay；
- 配置`max_entry_bytes`；
- 超大ToolResult/reasoning未来可以引入private blob reference，但不能自动拆成多个model-visible fragments；
- streaming/progress不写入ledger，避免高频I/O和fork噪声；
- usage aggregate、session list和search index使用rebuildable cache。

## 与 Session Execution 的关系

Session execution拥有append sequencing和terminal arbitration，不把它们下推给writer。

推荐执行流：

```text
append TurnContext
→ append user/input
→ AgentLoop NeedModel
→ ModelGateway.generate
→ append assistant/intermediate
→ append Interaction events / ToolExecutionStarted as needed
→ execute Tools
→ append one tool message per truthful result
→ append ToolRoundCompleted
→ AgentLoop NeedModel
→ ...
→ append assistant/final
```

AgentLoop可以提前维护execution-local ToolResult和pending state，但下一次实际模型调用必须等待ToolRoundCompleted append/apply。

SessionWriter不决定：

- 何时调用模型；
- 何时执行Tool；
- Steer/FollowUp；
- cancel；
- Interaction deadline；
- terminal winner。

## 明确不建立

当前不建立：

```text
StoredSessionBatch
SessionWriteBatch
BatchId
batch fingerprint
batch commit marker/group protocol
Branch entity
ToolRound entity
MessageManager
InteractionService
通用database transaction hierarchy
chat_history + events dual log
content-addressed DAG
```

## 被否决的方案

### 一行一个业务batch

否决原因：将物理写入、history tree、ToolRound和terminal cleanup耦合为同一协议，增加BatchId/fingerprint/interior-entry/fork remap复杂度，不符合当前by-entry和历史fork目标。

### 一条content block一个entry

否决原因：会把同一个finalized assistant response拆成partial response，引入response group、usage重复、ordering和recovery复杂性。一个assistant response使用一个entry和ordered `content[]`。

### Message与Runtime Event双写同一内容

否决原因：Codex式ResponseItem + AgentMessage/UserMessage EventMsg产生重复事实。MiniCore message是canonical content，Runtime observer event从receipt派生。

### Generic durable Event/Custom Value

否决原因：未知payload可能影响recovery或conversation，却无法由storage validator理解。核心durable event保持typed且数量受控。

### chat history和operational events分两个文件

否决原因：形成dual durable truth，需要额外snapshot、ordering和repair协议。

### 自动补全partial ToolRound

否决原因：restart时缺少原AgentLoop/control/context，自动promotion可能让模型看到未经当前owner仲裁的旧结果。baseline保留truthful entries并Interrupted Turn。

## 基础不变量

- 一个Session只有一个authoritative JSONL entry tree；
- 一行一个complete StoredSessionEntry；
- runtime mutation只通过SessionWriter::append；
- operation key先于current-entry conflict lookup；
- same key同payload幂等返回，same key不同payload conflict；
- EntryId由writer分配；
- parent_id形成tree，current entry是最后成功append entry；
- TurnContext不创建Turn；
- input user message append开始Turn；
- final assistant message append完成Turn；
- Steer是source=Steer的user message；
- assistant response一个entry，content[]保存ordered reasoning/text/tool_call；
- finalized reasoning和usage随assistant保存；
- ToolCall创建Started ToolInvocation；
- Tool message保存truthful result并完成operational Item；
- ToolExecutionStarted先于side effect；
- Interaction request先于notify，resolution先于wake；
- assistant intermediate和tool message在ToolRoundCompleted前不model-visible；
- ToolRoundCompleted必须exact cover全部ToolCalls和Tool messages；
- incomplete ToolRound可以durable但不能进入conversation；
- outcome unknown不生成Tool message；
- Interrupted/Failed不生成synthetic assistant message；
- Prompt只消费CommittedConversationView；
- every append返回trusted AdvanceOnly/Append/Replace delta；
- projection mismatch时all-or-replay；
- compaction append overlay，不改写历史；
- fork deep copy selected parent path并remap nested refs；
- partial tail只允许最后一个未换行fragment；
- complete bad line或非法reference fail closed；
- recovery不恢复旧I/O、不自动重放Tool、不自动补ToolRoundCompleted；
- observer/cache/index不是第二事实来源。

## 测试矩阵

至少覆盖：

- context entry后crash但无user input；
- user input append OutcomeUnknown lookup；
- assistant intermediate含多个ToolCall；text/reasoning-only Intermediate被拒绝；
- reasoning text/encrypted/signature round-trip；
- usage/model/response ID/finish reason/allowlisted provider metadata round-trip；
- logical retry_count不混入ModelGateway transparent retry；
- Interaction request-before-notify；
- resolution-before-wake；
- ToolExecutionStarted before side effect；
- pre-execution Tool message无execution-start ref；
- executed Tool message exact execution-start ref；
- partial tool results无ToolRoundCompleted；
- ToolRoundCompleted missing/duplicate/reordered refs fail closed；
- ToolRoundCompleted后conversation一次append完整round；
- final assistant拒绝尚未被ToolRoundCompleted引用的Intermediate assistant；
- final assistant与Cancel/Steer race；
- Interrupted/Failed terminal cleanup；
- recovery保留existing tool messages、不补promotion；
- late completion/late Interaction resolution；
- duplicate operation key same/different payload；
- write/flush OutcomeUnknown poisons old writer，reopen后分别覆盖entry存在与partial-tail不存在；
- partial final line；
- newline-terminated bad line；
- parent cycle/missing parent；
- fork before/after user/final assistant；Before Input同时排除associated TurnContext；
- fork mid-Turn target-local interruption；
- nested EntryId/TurnId/ItemId/RequestId remap；
- copied historicalTurnContext保留source-scoped exact definition refs，child future definition独立；
- compaction source fingerprint和protected complete ToolRound；
- SummaryModel compaction的TurnModelRef、usage、finish reason、logical retry count和provider metadata round-trip；
- Prompt永远看不到uncompleted ToolRound。

## 当前开放问题

仍需后续阶段闭合：

1. exact Rust serde tags/field casing，以及未来format v2+ migration policy；
2. Rig/AgentLoop adapter如何映射一个finalized assistant response及ordered content；
3. Rig adapter如何提取各provider的finish reason、reasoning artifact和allowlisted response metadata；
4. max entry size及未来blob reference阈值；
5. cold exact resume是否值得在稳定executor implementation identity后扩展。

## 完成检查

- [x] 选择per-session append-only by-entry JSONL tree。
- [x] 固定SessionHeader create/fork staging exception。
- [x] 定义StoredSessionEntry record、EntryId、parent_id和operation key。
- [x] 固定TurnContext / Message / Event / Compaction顶层类别。
- [x] 固定user/assistant/tool message layout。
- [x] 固定assistant finalized response聚合、reasoning和usage持久化。
- [x] 定义durable Interaction和Tool side-effect events。
- [x] 定义ToolRoundCompleted committed-only conversation projection rule。
- [x] 定义CommittedConversationState/View/Delta和AdvanceOnly/Append/Replace。
- [x] 定义entry-level idempotency和OutcomeUnknown lookup。
- [x] 定义parent tree、fork deep copy和identity remap。
- [x] 定义compaction overlay。
- [x] 定义partial tail、strict corruption和conservative recovery。
- [x] 保持SessionStorage为唯一durable truth。
- [x] 完成SessionExecutor和entry append sequencing。
- [x] 完成ModelGateway normalization与persistence contract设计。
- [x] 完成public ForkAnchor payload设计，见[Runtime Interface](runtime-interface.md)。
- [ ] 完成ModelGateway与AgentLoop adapter实现。
- [ ] 完成实现、fixture和property tests。
