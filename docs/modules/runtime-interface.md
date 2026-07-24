# Runtime Interface 与公开协议架构设计

日期：2026-07-24

状态：当前权威架构（设计已冻结，实现进行中）

## 目的

本文定义 MiniCore 的公开 Runtime interface，回答：

- 外部 CLI、TUI、Tauri host 或其他 adapter 如何调用 MiniCore；
- Command、Query、Snapshot 和 Event 各自负责什么；
- Agent、Session、Turn、Item 和 Interaction 如何进入公开 payload；
- Command 何时返回、异步业务完成如何通知；
- SessionStorage message tree 如何通过 Runtime 读取和操作；
- streaming progress 与可靠状态变化如何分离；
- 多 loaded Session 如何订阅、恢复和避免全 Runtime snapshot barrier；
- slash command 和 GUI command palette 如何共享 CommandSurface；
- transport、protocol version、capability、redaction 和兼容策略；
- 哪些内部对象永远不能进入公开协议。

相关权威文档：

- [MiniCore 领域模型](../architecture.md)
- [Agent 与 Session 生命周期](agent-session-lifecycle.md)
- [Turn、Item 与 Interaction](turn-item-interaction.md)
- [Conversation 与 SessionStorage](conversation-storage.md)
- [Session Execution](session-execution.md)
- [ModelGateway](model-gateway.md)
- [Compaction](compaction.md)

## 非目标

本文不定义：

- CLI、TUI、Vue、Tauri command 或 widget 的具体实现；
- JSON-RPC、WebSocket、Tauri IPC 的完整 wire schema；
- 独立 daemon、跨进程长期 event replay 或 multi-client authorization；
- provider、Rig、Tool executor、PromptSet 或 SessionStorage 的公开调用入口；
- UI selected Session、输入框草稿、滚动位置、窗口布局或折叠状态；
- standalone/manual `CompactSession`；
- Extension / Plugin 协议。

## 决策摘要

- `MiniCoreRuntime` 是外部宿主接触 MiniCore 的唯一顶层门面；
- 公开 interface 固定为 `dispatch`、`query`、`snapshot` 和 `subscribe` 四类能力；
- Command 修改事实或启动工作；Query 只读；Snapshot 恢复当前读模型；Event 通知状态变化和进度；
- 公开领域 identity 使用 `AgentId → SessionId → TurnId → ItemId → RequestId`；
- 不定义公开 `RunId` 或 `WorkspaceId`；
- `CommandId` 只做协议命令 correlation和幂等，不是领域 entity；Starting admission控制使用process-local `SubmissionId`；
- Command response 在对应 command 的明确线性化点返回 typed outcome，不使用只有 `accepted: bool` 的通用 acknowledgement；
- Turn 的长期完成、Item 生命周期和 Interaction request/resolution通过 Event 发布；
- 可靠 `StateEvent` 与可合并/丢弃的 `ProgressEvent` 分离；
- recovery cursor 只覆盖可靠 StateEvent，ProgressEvent 不占用 cursor；
- Runtime scope 和每个 Session scope 使用独立 cursor；不建立 runtime-global event sequence；
- `RuntimeSnapshot` 只覆盖 Runtime/Agent/Session summary 和 loaded membership；
- `SessionSnapshot` 覆盖一个 loaded Session 的 current Turn、Items、Pending Interaction 和 queues；
- 不要求 all-loaded Session 在一个全局水位上 stop-the-world snapshot；
- SessionStorage 拥有 durable entry tree；Runtime 通过 Query 暴露 read model，通过 Fork command创建新 Session branch；
- 同一 Session 内不提供原地 checkout/navigation mutation；
- CommandSurface 是 MiniCoreRuntime 内部无状态命令解释模块；slash command 只是 command text 的一种语法；
- 所有改变 Runtime 事实的 UI 操作都经过 MiniCoreRuntime；纯 UI 状态留在 adapter；
- 公开 interface 是 in-process Rust interface 和 transport-neutral serde types；具体 transport 使用薄 adapter。

## 同类产品取舍

| 产品 | 公开形状 | MiniCore 取舍 |
| --- | --- | --- |
| Codex App Server | bidirectional JSON-RPC；Thread → Turn → Item；request返回identity，notification发布后续状态；approval使用server request | 采用领域层级、短请求加事件流、显式Turn控制；不复制其process-global workspace和大量transport method |
| Agent Client Protocol | session/prompt长请求；session/update通知；client反向处理permission和filesystem | 采用typed Interaction和capability思想；不让一个长请求持有整个Turn完成生命周期 |
| Claude Agent SDK | long-lived Query对象、async message iterator、control methods和callback | 采用嵌入式易用性；不把callback或SDK对象句柄固化为wire contract |
| LangGraph Agent Server | Thread/Run资源、durable task queue、checkpoint、SSE join/cancel | 独立daemon出现后再评估Run resource和durable replay；不支付分布式队列成本 |
| OpenHands | SDK与Agent Server分层；append-only events同时服务memory和integration | 采用SDK/transport分层；继续让SessionStorage durable truth与公开observer event分离 |

## 顶层 Ownership

```text
External host
└─ MiniCoreRuntime public interface
   ├─ dispatch(CommandRequest)
   ├─ query(RuntimeQuery)
   ├─ snapshot(SnapshotRequest)
   └─ subscribe(SubscriptionRequest)

MiniCoreRuntime private implementation
├─ Agent / Session durable owners
├─ LoadedSessionExecutors
├─ CommandManager
├─ PromptService
├─ ToolService
├─ SkillService
├─ ModelGateway
├─ WorkspaceResolver / WorkspaceAuthority adapters
├─ SessionStorage / SessionWriter
├─ runtime state publisher
└─ per-session state/progress publishers
```

外部宿主不能取得 `SessionExecutionHandle`。Runtime 根据 command/query/snapshot scope 在内部定位 owner，并通过 private handle、store 或 Service 完成操作。

## Public Interface

推荐 Rust interface：

```rust
#[async_trait]
pub trait MiniCoreRuntimeInterface: Send + Sync {
    async fn dispatch(
        &self,
        request: CommandRequest,
    ) -> Result<CommandResponse, RuntimeDispatchError>;

    async fn query(
        &self,
        query: RuntimeQuery,
    ) -> Result<QueryResponse, QueryError>;

    async fn snapshot(
        &self,
        request: SnapshotRequest,
    ) -> Result<SnapshotResponse, SnapshotError>;

    fn subscribe(
        &self,
        request: SubscriptionRequest,
    ) -> Result<EventStream, SubscriptionError>;
}
```

四个 entry point 是同一个深模块的公开 interface。它们共享 identity、error、view、revision 和 redaction 类型，但职责不重叠。

### Capability Matrix

| 能力 | 修改状态 | 启动异步工作 | 直接返回业务数据 | 产生StateEvent | 产生ProgressEvent |
| --- | --- | --- | --- | --- | --- |
| Command | 可以 | 可以 | 只返回typed command outcome或CommandOutput | 可以 | 间接可以 |
| Query | 不可以 | 不可以 | 可以 | 不可以 | 不可以 |
| Snapshot | 不可以 | 不可以 | 返回恢复读模型 | 不可以 | 不可以 |
| Subscribe | 不可以 | 不可以 | 返回stream handle | 消费 | 消费 |

等待 Session 进入 future state 不属于 Query。公开协议不提供 `WaitForIdle`。CLI/test adapter 可以在 subscribe 之上提供本地 `wait_for_turn_terminal` helper。

## Identity 与 Route

公开领域层级：

```text
MiniCoreRuntime
└─ AgentId
   └─ SessionId
      └─ TurnId
         └─ ItemId
            └─ RequestId
```

其他 identity：

- `AgentRevision`：immutable AgentDefinition revision；
- `SessionDefinitionRevision`：future Turn definition的原子revision；
- `ToolCallId`：provider/tool protocol correlation，不替代ItemId；
- `EntryId`：SessionStorage identity，默认不作为普通UI mutation input；
- `CommandId`：public command correlation和幂等identity；
- `SubmissionId`：Submit在领域Turn创建前的process-local admission control identity；
- `RuntimeCursor` / `SessionCursor`：可靠StateEvent恢复水位。

不公开：

- `RunId`；
- `WorkspaceId`；
- `execution_version`；
- `OperationType`；
- provider attempt id；
- Tool executor route；
- Workspace authorization lease id。

### Event Route

```rust
pub enum EventRoute {
    Runtime,
    Agent {
        agent_id: AgentId,
    },
    Session {
        session_id: SessionId,
    },
    Turn {
        session_id: SessionId,
        turn_id: TurnId,
    },
    Item {
        session_id: SessionId,
        turn_id: TurnId,
        item_id: ItemId,
    },
    Interaction {
        session_id: SessionId,
        turn_id: TurnId,
        item_id: ItemId,
        request_id: RequestId,
    },
}
```

Rust内部优先使用route enum，使非法坐标组合无法构造。Wire adapter可以投影为flat fields，但必须保持单一权威route。

## Command

Command 表达 mutation、lifecycle operation、Turn control或用户命令解释请求。

### Command Envelope

```rust
pub struct CommandRequest {
    pub command_id: CommandId,
    pub command: RuntimeCommand,
}
```

`CommandId`由调用adapter在dispatch前生成。相同CommandId与相同normalized payload重试必须幂等；相同CommandId携带不同payload返回`CommandConflict`。每个Command使用语义明确的expected revision、expected status或expected TurnId表达乐观并发。

### RuntimeCommand

```rust
pub enum RuntimeCommand {
    Agent(AgentCommand),
    Session(SessionCommand),
    Turn(TurnCommand),
    Interaction(InteractionCommand),
    CommandSurface(CommandSurfaceCommand),
}
```

### AgentCommand

```rust
pub enum AgentCommand {
    Create {
        definition: NewAgentDefinition,
        metadata: NewAgentMetadata,
    },
    UpdateDefinition {
        agent_id: AgentId,
        expected_revision: AgentRevision,
        patch: AgentDefinitionPatch,
    },
    UpdateMetadata {
        agent_id: AgentId,
        expected_revision: AgentMetadataRevision,
        patch: AgentMetadataPatch,
    },
    SetStatus {
        agent_id: AgentId,
        expected_status: AgentStatus,
        status: AgentUsableStatus,
    },
    Delete {
        agent_id: AgentId,
        expected_status: AgentStatus,
    },
}
```

`AgentUsableStatus`只允许`Enabled | Disabled`。`AgentStatus::Deleted`只能通过`Delete`进入，Deleted identity不复用。

### SessionCommand

```rust
pub enum SessionCommand {
    Create {
        agent_id: AgentId,
        definition: NewSessionDefinition,
        metadata: NewSessionMetadata,
    },
    UpdateDefinition {
        session_id: SessionId,
        expected_revision: SessionDefinitionRevision,
        patch: SessionDefinitionPatch,
    },
    UpgradeAgentRevision {
        session_id: SessionId,
        expected_revision: SessionDefinitionRevision,
        target: Option<AgentRevisionRef>,
    },
    UpdateMetadata {
        session_id: SessionId,
        expected_revision: SessionMetadataRevision,
        patch: SessionMetadataPatch,
    },
    Load {
        session_id: SessionId,
    },
    Unload {
        session_id: SessionId,
    },
    Archive {
        session_id: SessionId,
    },
    Unarchive {
        session_id: SessionId,
    },
    Delete {
        session_id: SessionId,
    },
    Fork {
        source_session_id: SessionId,
        anchor: ForkAnchor,
    },
}
```

`SessionDefinitionPatch`原子修改 Workspace、SessionModelConfig 或 SessionPrompts，并生成新的 `SessionDefinitionRevision`。修改Agent reference必须走`UpgradeAgentRevision`。

`Create` 只接受 `agent_id`：Runtime 在创建的 Agent lifecycle synchronization 内读取该 Agent 当时的 current revision，并把它作为 exact `AgentRevisionRef` 钉进 `SessionDefinition`。调用方不在 create 时报 revision——「用哪一版」由 Runtime 在此刻快照 current 决定，之后 Agent 再发布新 revision 不会改变该 Session（snapshot-current）。

`UpgradeAgentRevision.target` 为 `Option`：缺省（`None`）表示「重新钉到该 Agent 当前 current」（显式 reload 升级），是常规路径；给出 exact `AgentRevisionRef` 表示钉到指定版本（可用于钉旧版或回滚）。两种情况都在 gates 内校验 target 属于同一 AgentId、Agent 为 Enabled、target definition 存在，并原子解析为 exact ref 后写入新的 `SessionDefinitionRevision`；`latest` 本身不进入 durable `SessionDefinition`。保持 exact pin 让同一 Session 在两次 upgrade 之间上下文稳定，最大化 prompt cache 前缀命中。

同一Session不提供原地history checkout。创建历史分支使用`Fork`，得到新的SessionId和独立definition revision序列。

### ForkAnchor

普通UI使用message/item anchor，不直接构造EntryId：

```rust
pub enum ForkAnchor {
    Genesis,
    BeforeUserMessage {
        item_id: ItemId,
    },
    AfterUserMessage {
        item_id: ItemId,
    },
    BeforeFinalAgentMessage {
        item_id: ItemId,
    },
    AfterFinalAgentMessage {
        item_id: ItemId,
    },
}
```

Runtime把公开anchor解析为合法storage path end：

- `BeforeUserMessage(Input)`同时排除associated TurnContext；
- `BeforeUserMessage(Steer)`解析到该message的parent；
- `AfterUserMessage`包含对应durable UserMessage；
- `Before/AfterFinalAgentMessage`只接受`phase = Final`的AgentMessage Item；
- anchor产生non-terminal tail Turn时，fork staging按`HistoricalFork`规则关闭；
- intermediate AgentMessage、Reasoning、ToolInvocation、Interaction和裸ToolResult不作为普通UI anchor；
- cross-session、stale或kind不匹配的ItemId返回typed error。

### TurnCommand

```rust
pub enum TurnCommand {
    Submit {
        session_id: SessionId,
        submission_id: SubmissionId,
        intent: PromptIntentInput,
    },
    Steer {
        session_id: SessionId,
        expected_turn_id: TurnId,
        intent: PromptIntentInput,
    },
    FollowUp {
        session_id: SessionId,
        intent: PromptIntentInput,
    },
    Cancel {
        session_id: SessionId,
        target: PublicCancelTarget,
        reason: UserCancelReason,
    },
}

pub enum PublicCancelTarget {
    Submission(SubmissionId),
    Turn(TurnId),
}
```

语义：

- `Submit`只在Session可以admit新Turn时使用；initiating UserMessage append/apply后返回`TurnStarted`；
- `Steer`只作用于expected Running Turn；返回`Applied`或`Queued`；
- `FollowUp`进入bounded process-local FIFO；返回`Queued`；
- `Cancel(SubmissionId)`允许取消尚处于Starting admission的Submit；
- `Cancel(TurnId)`在terminal cleanup完成后返回；
- Turn terminal后迟到Steer或Cancel返回typed stale/terminal outcome。

Queued Steer和FollowUp仍是process-local值。Runtime restart后未append的queued input不会恢复。

### InteractionCommand

```rust
pub enum InteractionCommand {
    Resolve {
        session_id: SessionId,
        expected_turn_id: TurnId,
        item_id: ItemId,
        request_id: RequestId,
        resolution: InteractionResolutionInput,
        resolution_key: IdempotencyKey,
    },
}
```

Interaction request/resolution遵守：

```text
InteractionRequested append/apply
→ StateEvent::InteractionRequested
→ host提交Resolve
→ InteractionResolved append/apply
→ StateEvent::InteractionResolved
→ resume waiter或允许后续side effect
```

结构化Interaction answer不是UserMessage，不开启新Turn。

### CommandSurfaceCommand

```rust
pub enum CommandSurfaceCommand {
    ExecuteText {
        session_id: Option<SessionId>,
        raw: String,
        delivery: Option<CommandPromptDelivery>,
    },
    ExecuteCatalog {
        session_id: Option<SessionId>,
        selection: CommandSelection,
        args: CommandArgs,
        delivery: Option<CommandPromptDelivery>,
    },
}

pub enum CommandPromptDelivery {
    Submit,
    Steer {
        expected_turn_id: TurnId,
    },
    FollowUp,
}
```

`delivery`只对产生PromptIntent的command生效。`/status`、`/help`、`/model`等command忽略该字段。

### Command Response

```rust
pub struct CommandResponse {
    pub command_id: CommandId,
    pub outcome: CommandOutcome,
    pub output: Option<CommandOutput>,
}

pub enum CommandOutcome {
    AgentCreated {
        agent_id: AgentId,
        revision: AgentRevision,
    },
    AgentUpdated {
        revision: AgentRevision,
    },
    SessionCreated {
        session_id: SessionId,
        revision: SessionDefinitionRevision,
    },
    SessionLoaded,
    SessionUnloaded,
    SessionUpdated {
        revision: SessionDefinitionRevision,
    },
    SessionForked {
        session_id: SessionId,
    },
    TurnStarted {
        turn_id: TurnId,
    },
    SteerApplied {
        turn_id: TurnId,
    },
    SteerQueued {
        turn_id: TurnId,
    },
    FollowUpQueued,
    InteractionResolved,
    Cancelled,
    CommandOutput,
    NoChange,
}
```

Command response不是完整业务完成流：

- `TurnStarted`只表示领域Turn已由initiating UserMessage append创建；
- Turn最终`Completed | Interrupted | Failed`由StateEvent发布；
- `SteerQueued`和`FollowUpQueued`不承诺crash-safe delivery；
- Session load可以在load/recovery完成后返回typed loaded/readiness outcome；
- slash command的display-neutral结果可以直接放入`CommandOutput`；
- Session/Agent事实变化同时发布StateEvent，让其他subscriber失效或更新read model。

### Command 线性化点

| Command | Response线性化点 |
| --- | --- |
| Create Agent | Agent head与revision durable publication |
| Update Agent | expected revision/status CAS成功 |
| Create Session | SessionHeader/definition durable publication |
| Update SessionDefinition | new revision durable publication |
| Load Session | single-flight load/recovery完成并发布readiness |
| Unload Session | SessionExecutor进入Idle并从loaded map移除 |
| Submit | initiating UserMessage append/apply |
| Steer Applied | Steer UserMessage append/apply |
| Steer Queued | pending Steer FIFO admission |
| FollowUp | FollowUp FIFO admission |
| Resolve Interaction | InteractionResolved append/apply |
| Cancel Submission | Starting candidate取消完成，且不会创建领域Turn |
| Cancel Turn | Turn terminal append/apply与cleanup完成 |
| Fork Session | target staging验证完成并原子发布 |

### Command Error

```rust
pub struct CommandError {
    pub code: CommandErrorCode,
    pub message: String,
    pub retry: RetryAdvice,
    pub subject: Option<PublicSubject>,
}
```

主要分类：

```text
InvalidArgument
NotFound
CommandConflict
StaleRevision
AgentDisabled
AgentDeleted
SessionArchived
SessionDeleted
SessionNotLoaded
SessionNotReady
SessionBusy
RequestQueueFull
ExpectedTurnMismatch
TurnNotRunning
TurnTerminal
InteractionNotFound
InteractionAlreadyResolved
InteractionExpired
InteractionFamilyMismatch
InvalidForkAnchor
Unauthorized
Unavailable
DurableStateCorrupt
RuntimeClosing
```

外部调用方不能解析自然语言message决定retry。`RetryAdvice`使用typed值，例如`DoNotRetry | RefreshAndRetry | RetryWithBackoff | UserActionRequired`。

## CommandSurface

`CommandSurface`是用户命令领域面，位于MiniCoreRuntime内部：

```text
ExecuteText / ExecuteCatalog
→ build explicit CommandContext
→ CommandManager.materialize()
→ parse / suggest / resolve_for_execution
→ trusted handler binding
→ ResolvedCommandAction
→ MiniCoreRuntime route to owning command/query operation
```

### Ownership

```text
MiniCoreRuntime
└─ CommandManager                 // shared, stateless
   ├─ CommandPackStore
   ├─ CandidateProviderRegistry
   ├─ HandlerRegistry
   ├─ Materializer
   ├─ Parser
   ├─ Suggester
   └─ Resolver
```

`CommandManager`无状态，支持递归command tree、dynamic candidate、执行前重新resolve和UI-safe catalog。

MiniCoreRuntime不持有长期command facade。MiniCoreRuntime每次基于显式optional SessionId构造`CommandContext`；CommandManager不持有SessionExecutor handle或current Session。

### Handler Output

```rust
pub enum ResolvedCommandAction {
    Dispatch(RuntimeCommand),
    Read(RuntimeQuery),
    Present(CommandOutput),
    Prompt {
        intent: PromptIntentInput,
        delivery: CommandPromptDelivery,
    },
}
```

Handler不能：

- 直接修改SessionExecutor；
- 直接写SessionStorage；
- 直接调用ModelGateway或Tool executor；
- 携带完整高权限RuntimeCommand进入UI catalog；
- 读取credential、raw provider payload、Skill正文或完整Prompt正文。

### Slash Command Examples

```text
/model provider openai gpt-5
→ resolve model candidate
→ SessionCommand::UpdateDefinition {
     expected_revision,
     patch: SessionModelConfig(model selection)
   }
→ new SessionDefinitionRevision
```

```text
/model thinking high
或 /thinking high
→ validate current model supported reasoning levels
→ SessionCommand::UpdateDefinition {
     expected_revision,
     patch: SessionModelConfig(reasoning preference)
   }
→ new SessionDefinitionRevision
```

active Turn继续使用已pin的TurnModelSnapshot；新revision只影响future Turn。

```text
/skill code-review 修复这个问题
→ PromptIntentInput::Skill
→ delivery选择Submit、Steer或FollowUp
→ 对应TurnCommand
```

```text
/status
→ RuntimeQuery或SessionQuery
→ CommandOutput
```

CommandSurface不注册`/compact`。automatic compaction由SessionExecutor在NeedModel安全点内部触发。

### Command Catalog

Command catalog是UI-safe read model，不是执行授权。UI可以查询catalog/suggestion并渲染slash autocomplete、command palette或菜单；执行时Runtime必须重新materialize和resolve。

同一command可以来自：

- slash text；
- GUI command palette；
- TUI nested menu；
- structured catalog selection。

所有入口最终走相同的resolver和handler binding。

## Query

Query提供typed、只读、立即request/response数据。

```rust
pub enum RuntimeQuery {
    Runtime(RuntimeReadQuery),
    Agent(AgentQuery),
    Session(SessionQuery),
    CommandSurface(CommandSurfaceQuery),
    Model(ModelQuery),
    Prompt(PromptQuery),
    Skill(SkillQuery),
    Tool(ToolQuery),
    Usage(UsageQuery),
    Diagnostics(DiagnosticsQuery),
}
```

### RuntimeReadQuery

```text
GetCapabilities
GetRuntimeInfo
ListLoadedSessions
```

### AgentQuery

```text
ListAgents
GetAgent
ListAgentRevisions
GetAgentRevision
```

### SessionQuery

```text
ListSessions
GetSession
GetSessionDefinition
GetSessionReadiness
GetHistoryTree
ListTurns
GetTurn
ListItems
GetItem
ListPendingInteractions
```

`GetHistoryTree`返回公开read model：

```rust
pub struct SessionHistoryTreeView {
    pub session_id: SessionId,
    pub current_anchor: Option<HistoryAnchorView>,
    pub nodes: Vec<HistoryNodeView>,
    pub revision: SessionHistoryRevision,
}
```

History node使用Turn/Item/message语义，不向普通UI暴露raw SessionEntryDraft、writer internals、ToolRoundCompleted内部event或writer current EntryId。

大列表必须分页。Cursor opaque并绑定query family、filter、sort和revision。

### CommandSurfaceQuery

```text
GetCatalog { session_id? }
Suggest { session_id?, input, cursor_position }
GetHelp { session_id?, command_path }
```

### ModelQuery

```text
ListProviders
ListModels { provider_id? }
GetModelCapabilities
```

只返回safe model identity、display metadata、availability、context limit和capabilities。Endpoint、auth reference、credential和provider client不公开。

### Prompt / Skill / Tool Query

只提供UI-safe catalog和diagnostics：

```text
Prompt: ListPromptTemplates / GetPromptTemplateSummary
Skill:  ListSkills / GetSkillSummary
Tool:   ListTools / GetToolSummary
```

正文默认不进入普通Query。后续privileged debug interface必须独立授权。

### Query Response

```rust
pub struct QueryResponse {
    pub stamp: ReadStamp,
    pub revision: Option<QueryRevision>,
    pub data: QueryResult,
}

pub struct ReadStamp {
    pub runtime_cursor: Option<RuntimeCursor>,
    pub session_cursor: Option<SessionCursor>,
}
```

Query不增加cursor，不发布Event，不创建Turn，不加载完整SessionExecutor来读取持久化目录，不消费Steer/FollowUp queue。

## Snapshot

Snapshot用于UI初始化、subscriber重建和StateEvent gap恢复。

```rust
pub enum SnapshotRequest {
    Runtime,
    Session {
        session_id: SessionId,
    },
}

pub enum SnapshotResponse {
    Runtime(RuntimeSnapshot),
    Session(SessionSnapshot),
}
```

### RuntimeSnapshot

```rust
pub struct RuntimeSnapshot {
    pub cursor: RuntimeCursor,
    pub runtime: RuntimeView,
    pub agents: Vec<AgentSummary>,
    pub loaded_sessions: Vec<LoadedSessionSummary>,
    pub catalogs: RuntimeCatalogRevisions,
    pub diagnostics: Vec<RuntimeDiagnosticView>,
}
```

`RuntimeSnapshot`不包含所有loaded Session的完整message、current Items或Pending Interaction。它只负责Runtime scope状态和loaded membership。

### SessionSnapshot

```rust
pub struct SessionSnapshot {
    pub session_id: SessionId,
    pub cursor: SessionCursor,
    pub lifecycle: SessionLifecycleView,
    pub definition: SessionDefinitionSummary,
    pub load_state: SessionLoadState,
    pub readiness: SessionReadiness,
    pub execution: SessionExecutionView,
    pub current_turn: Option<CurrentTurnView>,
    pub active_items: Vec<ItemView>,
    pub pending_interactions: Vec<InteractionView>,
    pub queues: SessionQueueView,
    pub usage: Option<SessionUsageView>,
    pub diagnostics: Vec<SessionDiagnosticView>,
}
```

完整历史通过Query分页读取。Snapshot只携带恢复当前执行UI所需状态。

### Snapshot Consistency

Runtime与Session使用独立owner和cursor：

```text
RuntimeSnapshot at RuntimeCursor R
SessionSnapshot A at SessionCursor A:17
SessionSnapshot B at SessionCursor B:42
```

不存在跨scope可比较的全局sequence。

`SessionSnapshot`请求通过对应SessionExecutionHandle进入SessionRequestQueue，因此与该Session之前处理的mutation和operation result有明确顺序。

`RuntimeSnapshot`由Runtime owner捕获Agent/Session membership和runtime projection。它不等待所有SessionExecutor parked，也不构造all-loaded stop-the-world barrier。

UI恢复一个Session：

```text
snapshot(SessionId) → cursor N
subscribe(SessionId, after N)
ignore state events <= N
```

若snapshot与subscribe之间发生变化，subscription从bounded reliable buffer补发；buffer不足返回Gap，调用方重新snapshot。

## Event

Event从Runtime通知host。Host不能通过Event反向修改Runtime；mutation仍使用Command。

### Subscription Scope

```rust
pub enum SubscriptionScope {
    Runtime,
    Session {
        session_id: SessionId,
    },
}

pub enum EventCursor {
    Runtime(RuntimeCursor),
    Session(SessionCursor),
}

pub struct SubscriptionRequest {
    pub scope: SubscriptionScope,
    pub after: Option<EventCursor>,
    pub include_progress: bool,
}
```

Runtime scope发布：

- Agent lifecycle和revision变化；
- Session catalog、load/unload membership和summary变化；
- runtime catalog invalidation；
- runtime diagnostics。

Session scope发布：

- Session definition/readiness/execution变化；
- Turn lifecycle；
- Item lifecycle；
- Interaction request/resolution；
- queues、usage和session diagnostics；
- model/tool progress。

### EventFrame

```rust
pub enum EventFrame {
    State(StateEvent),
    Progress(ProgressEvent),
    Gap(EventGap),
    Closed(SubscriptionClosed),
}
```

### StateEvent

```rust
pub struct StateEvent {
    pub event_id: EventId,
    pub cursor: ScopedCursor,
    pub timestamp: Timestamp,
    pub command_id: Option<CommandId>,
    pub route: EventRoute,
    pub msg: StateEventMsg,
}
```

StateEvent规则：

- scope内cursor严格单调；
- 一旦分配cursor，subscriber delivery不能静默丢弃；
- subscriber queue无法继续时发送Gap/Closed，调用方使用Snapshot恢复；
- durable conversation fact必须从append/apply后的CommittedSessionEntry派生；
- process-local load/readiness/queue事实必须能从对应Snapshot重建；
- payload包含完整final view，能够校正之前丢失的ProgressEvent。

主要event family：

```text
agent_created
agent_definition_updated
agent_status_changed
session_created
session_loaded
session_unloaded
session_definition_updated
session_archived
session_unarchived
session_deleted
session_forked
session_readiness_changed
session_execution_changed
session_settled
turn_started
turn_phase_changed
turn_completed
turn_interrupted
turn_failed
item_completed
item_tool_invocation_started
item_tool_invocation_completed
item_tool_invocation_abandoned
interaction_requested
interaction_resolved
queue_updated
usage_updated
diagnostics_updated
command_catalog_invalidated
```

Turn terminal使用三个互斥event type，或wire adapter使用一个`turn_finished { status }`。同一protocol major内只能选择一种稳定wire形状；Rust领域payload保持`Completed | Interrupted | Failed` typed union。

### ProgressEvent

```rust
pub struct ProgressEvent {
    pub timestamp: Timestamp,
    pub route: EventRoute,
    pub kind: ProgressEventKind,
    pub update: ProgressUpdate,
}
```

ProgressEvent不携带RuntimeCursor或SessionCursor。典型内容：

```text
agent_message_delta
reasoning_delta
tool_output_delta
model_retry_scheduled
model_attempt_progress
```

规则：

- 可以按SessionId/TurnId/ItemId合并连续delta；
- queue满时可以丢弃中间progress；
- progress缺失不触发StateEvent gap；
- Item/Turn最终StateEvent携带完整final view；
- progress不写SessionStorage，不成为conversation truth；
- progress publisher失败不影响Turn执行或terminal。

### Interaction Event

`interaction_requested`必须携带UI-safe request：

```rust
pub struct InteractionView {
    pub request_id: RequestId,
    pub turn_id: TurnId,
    pub item_id: ItemId,
    pub request: InteractionRequestView,
    pub state: InteractionStateView,
    pub expires_at: Option<Timestamp>,
}
```

prepared Tool args、executor handle、sandbox internals和credential不进入event。Host回答时必须回传SessionId、expected TurnId、ItemId、RequestId和resolution key。

## Message Tree 管理

Session message/history tree由MiniCore内部管理：

```text
SessionStorage
├─ owns EntryId + parent_id immutable tree
├─ owns current physical entry and replay
├─ validates cross-entry references
└─ produces trusted projections

SessionExecutor
├─ performs append → apply
├─ owns active Turn mutation ordering
└─ requests SessionStorage branch/fork operations

MiniCoreRuntime
├─ exposes SessionQuery::GetHistoryTree / ListTurns / ListItems
└─ exposes SessionCommand::Fork
```

UI不能：

- 直接读取或修改JSONL；
- 直接指定任意EntryId作为writer parent；
- 追加raw message；
- 补写ToolResult或ToolRoundCompleted；
- 删除历史entry；
- 修改current leaf pointer。

公开能力：

- 分页读取history tree、Turn和Item read model；
- 使用Genesis、UserMessage或FinalAgentMessage anchor创建Fork Session；
- 查询fork provenance；
- 在新Session继续future Turn。

同一Session原地navigation/checkout需要额外定义head mutation、active Turn conflict、compaction overlay和event语义，留到出现真实产品需求后设计。

## Agent 与 Session 管理

### Agent

MiniCoreRuntime路由Agent command到durable Agent owner：

```text
Create/Update/Status/Delete
→ Agent lifecycle synchronization
→ durable head/revision publication
→ Runtime StateEvent
```

Session保存exact AgentRevisionRef。Agent发布新revision不会自动改变existing Session。

### Session

MiniCoreRuntime维护：

```text
persistent Session catalog
loaded SessionId → private SessionExecutionHandle
```

规则：

- 一个loaded Session正好一个SessionExecutor；
- 不同SessionExecutor可以并行Running；
- public command显式携带SessionId；
- Runtime不保存selected/current Session；
- load single-flight；
- unload先走PrepareForUnload；
- SessionDefinition update使用expected revision；
- active Turn继续pin旧definition；future Turn使用新revision；
- archive/delete与load/admission按lifecycle规则线性化。

## UI Boundary

所有影响MiniCore事实的UI操作经过Runtime facade：

```text
创建/更新Agent
创建、加载、归档或Fork Session
提交、Steer、FollowUp或Cancel Turn
回答Interaction
修改Session model/reasoning/workspace/prompts
执行slash/catalog command
```

UI本地管理：

```text
selected Session
editor draft / cursor / undo
scroll position
expanded/collapsed Item
window layout
local keyboard shortcuts
optimistic visual affordance
```

Host可以提供本地UI command overlay，但不能shadow同名Runtime command，也不能绕过Runtime mutation。

后续协议扩展必须由真实领域能力驱动，例如history checkout、session export或credential management。Button、modal、picker、toast和页面结构不进入Runtime protocol。

## 常规调用路径

### Submit 到模型请求

```text
CLI / TUI / Tauri backend
→ MiniCoreRuntime.dispatch(TurnCommand::Submit)
→ Runtime校验Session lifecycle/load/readiness
→ Runtime校验exact Agent status和SessionDefinitionRevision
→ SessionExecutionHandle
→ bounded SessionRequestQueue
→ SessionExecutor reserves candidate Turn
→ WorkspaceResolver.resolve()
→ ModelGateway.resolve_for_turn()
→ SkillService.catalog()
→ ToolService.for_turn()
→ PromptService.for_turn()
→ TurnExecutionContext
→ PromptSet.compose_user_message()
→ SessionWriter.append(TurnContext)
→ apply trusted projections
→ SessionWriter.append(UserMessage source=Input)
→ apply trusted projections
→ CommandResponse::TurnStarted { turn_id }
→ AgentLoop returns NeedModel
→ PromptSet.assemble(committed conversation)
→ AssembledModelContext
→ ModelCallRequest
→ ModelGateway.generate_model_turn()
→ private Rig/provider adapter
→ OpenAI Responses / Anthropic Messages / other provider
→ ModelCallResult
→ SessionExecutor validates SessionId + TurnId + execution_version
→ append/apply AgentMessage、Tool entries或Turn terminal
→ StateEvent + optional ProgressEvent
→ host reducer/UI
```

外部host不能在`PromptSet.assemble`与`ModelGateway.generate_model_turn`之间插入raw message或provider payload。

### Slash `/model`

```text
UI dispatch ExecuteText("/model provider openai gpt-5")
→ MiniCoreRuntime
→ CommandManager materialize current session catalog
→ parse + resolve + validate model candidate
→ ResolvedCommandAction::Dispatch(SessionCommand::UpdateDefinition)
→ Session lifecycle owner CAS expected revision
→ durable new SessionDefinitionRevision
→ CommandResponse { revision, output }
→ session_definition_updated StateEvent
→ future Turn admission resolve new TurnModelSnapshot
```

### Tool Approval

```text
ModelCallResult contains ToolCall
→ SessionExecutor creates ToolInvocation ItemId
→ append/apply InteractionRequested
→ session StateEvent::InteractionRequested
→ UI dispatch InteractionCommand::Resolve
→ append/apply InteractionResolved
→ session StateEvent::InteractionResolved
→ ToolExecutionControl records ToolExecutionStarted
→ append/apply
→ executor side effect
→ append/apply truthful Tool result
→ append/apply tool_round_completed
→ next PromptSet.assemble
```

## Transport 与 Adapter

生产优先级：

1. in-process Rust调用；
2. Tauri backend在可信进程内调用同一interface；
3. 测试使用in-memory runtime和synthetic provider/tool adapters。

JSON-RPC、stdio或WebSocket adapter出现第二个真实transport需求后实现。

Transport adapter只负责：

- serialization/deserialization；
- transport request id correlation；
- connection lifecycle；
- frame size、backpressure和authentication；
- protocol initialize/version negotiation；
- EventStream到transport notification映射。

Transport adapter不负责：

- Session selection；
- command parse/authorization；
- event cursor生成；
- Session state；
- Tool approval policy；
- provider retry；
- storage truth。

Transport request id不能替代CommandId、TurnId、RequestId或cursor。

## Version 与 Capability

Rust public types使用一个明确protocol major。Wire adapter初始化时交换：

```rust
pub struct ProtocolHello {
    pub supported_versions: Vec<ProtocolVersion>,
    pub client: ClientInfo,
    pub capabilities: ClientCapabilities,
}

pub struct ProtocolWelcome {
    pub selected_version: ProtocolVersion,
    pub runtime: RuntimeInfo,
    pub capabilities: RuntimeCapabilities,
    pub limits: ProtocolLimits,
}
```

MVP capability示例：

```text
state_events
progress_events
runtime_snapshot
session_snapshot
paged_queries
command_catalog
interaction_resolution
session_fork
```

兼容规则：

- major改变允许breaking payload change；
- minor只做additive field、event或capability；
- optional feature必须通过capability协商；
- adapter不能根据runtime version字符串猜测字段；
- deprecated字段先标记并至少保留一个明确兼容周期；
- experimental capability不进入stable default surface。

Core in-process interface通过`RuntimeQuery::GetCapabilities`读取同一能力视图，不额外要求transport handshake。

## Security 与 Redaction

公开协议不得包含：

- API key、OAuth token、auth header；
- provider endpoint secret和raw response body；
- PromptSet完整system/developer instructions；
- Skill正文和Prompt template正文；
- Tool executor handle、prepared private args和sandbox internals；
- Workspace authorization lease；
- SessionWriter、writer internals、repair internals；
- raw JSONL path作为普通UI能力；
- internal handler id和完整RuntimeCommand嵌入catalog action。

Renderer不应直接持有MiniCoreRuntime credential或filesystem capability。Tauri/Vue架构中，可信backend调用Runtime，renderer只消费serializable safe view并提交受限Command。

## 内部对象禁止公开

以下类型永远留在crate内部：

```text
SessionExecutor
SessionExecutionHandle
SessionRequestQueue
RunningOperation
OperationResult
execution_version
AgentLoop
TurnExecutionContext
PromptService / PromptSet
ToolService / ToolSet
SkillService / SkillCatalog / LoadedSkill
WorkspaceSnapshot / authorization lease
ModelGateway / TurnModelSnapshot private execution ref
ProviderAdapter / AuthStore
SessionWriter / SessionStorage implementation
CommittedConversationState / Delta
ToolExecutionControl
```

公开view可以投影其安全状态，但不能携带内部handle或允许调用方拼接不一致快照。

## Error 分层

```text
RuntimeDispatchError
  协议请求无法进入Runtime，例如RuntimeClosed、invalid envelope、fatal unavailable

CommandError
  命令在领域或admission处被拒绝

QueryError
  只读请求invalid/not found/stale/unavailable

SnapshotError
  scope不存在、Session未loaded、snapshot unavailable

SubscriptionError
  invalid cursor、scope不支持、replay window不足

StateEvent
  已接受异步工作的后续业务失败，例如TurnFailed
```

Model、Tool和Storage内部错误必须归约成typed、redacted public failure。调用方不能解析provider字符串决定retry。

## Test Strategy

Public interface是 contract test surface。

### Command Tests

- Agent/Session revision CAS；
- Submit只在UserMessage append/apply后返回TurnStarted；
- Cancel(SubmissionId)关闭Starting admission；
- Steer expected TurnId与Applied/Queued；
- FollowUp bounded queue和restart loss；
- Interaction first committed resolution wins；
- Fork Before/After Item anchor；
- `/model`和`/thinking`生成new SessionDefinitionRevision；
- `/compact`不在catalog。

### Query/Snapshot Tests

- Query只读且不增加cursor；
- session list不加载SessionExecutor；
- history tree不暴露internal entry/event；
- SessionSnapshot通过request queue线性化；
- RuntimeSnapshot不等待所有SessionExecutor；
- pending Interaction可从SessionSnapshot恢复；
- cursor与scope不匹配时拒绝。

### Event Tests

- RuntimeCursor和每个SessionCursor独立单调；
- StateEvent不能静默丢弃；
- ProgressEvent可以合并/丢弃；
- progress丢失不产生state gap；
- final Item event携带完整view；
- Interaction request-after-append和resolution-before-resume；
- Turn只有一个terminal event；
- subscriber buffer不足返回Gap并可snapshot恢复。

### Security Tests

- catalog/query/snapshot/event不包含credential；
- renderer不能提交raw internal command；
- stale catalog selection重新resolve；
- cross-sessionItem/RequestId拒绝；
- Workspace restriction通过Runtime path生效。

## 方案比较

### 全局 Event Sequence + All-Loaded RuntimeSnapshot

优点：所有event有单一total order，reducer概念简单。

缺点：每次完整snapshot需要协调所有SessionExecutor；一个慢Session拖住全Runtime；filtered subscriber仍要处理无关gap；多Session之间通常没有业务因果关系。

结论：不采用。Runtime与Session使用独立cursor和snapshot。

### 一个长请求持有完整Turn

优点：CLI调用简单，response就是terminal result。

缺点：不利于后台多Session、Steer、Interaction、断线恢复和独立subscriber；transport request lifetime与领域Turn lifetime耦合。

结论：不采用。Command在短线性化点返回，Turn completion走Event。

### Generic Accepted Bool

优点：dispatch实现简单。

缺点：调用方无法知道revision、TurnId、Applied/Queued或Interaction resolution outcome；需要额外event猜测command结果。

结论：不采用。返回typed CommandOutcome。

### CommandSurface 由每个UI实现

优点：UI可以快速定制。

缺点：slash解析、catalog revision、权限、动态候选和handler语义漂移。

结论：不采用。Runtime拥有CommandSurface，UI只渲染和提交。

### Event 同时作为 Durable Conversation Log

优点：单一stream看似可以同时驱动模型与UI。

缺点：streaming/progress、provider retry和UI状态会污染Session durable truth；ToolRoundCompleted模型可见性无法安全表达。

结论：不采用。SessionStorage是durable truth，StateEvent从projection派生。

### 公开 Manual CompactSession

优点：用户可主动降低上下文占用。

缺点：需要standalone Session maintenance state、admission/queue/cancel和模型选择规则；Compaction只在active Turn NeedModel安全点定义完整语义。

结论：不采用。未来出现真实需求后独立设计。

## 实现顺序

1. 冻结public serde naming和protocol crate layout；
2. 实现Agent/Session durable command/query owner；
3. 实现MiniCoreRuntime dispatch routing；
4. 实现RuntimeCursor publisher和RuntimeSnapshot；
5. 实现per-session StateEvent publisher、SessionCursor和SessionSnapshot；
6. 实现ProgressEventPublisher adapter；
7. 实现CommandManager target migration和builtin command packs；
8. 实现history tree Query和ForkAnchor解析；
9. 接入SessionExecutor typed request response；
10. 接入ModelGateway catalog Query；
11. 编写public contract tests；
12. 实现首个in-process下游host adapter。

## 完成检查

- [x] 确定MiniCoreRuntime四类公开能力。
- [x] 确定Agent/Session/Turn/Item/Interaction公开identity。
- [x] 删除公开RunId和WorkspaceId。
- [x] 确定typed CommandOutcome和线性化点。
- [x] 确定CommandSurface ownership和slash/catalog统一入口。
- [x] 确定Query family和分页规则。
- [x] 确定RuntimeSnapshot与SessionSnapshot分域。
- [x] 确定RuntimeCursor与per-session SessionCursor。
- [x] 分离StateEvent与ProgressEvent。
- [x] 确定Interaction公开request/resolution。
- [x] 确定message tree Query和ForkAnchor。
- [x] 确定UI-local state边界。
- [x] 确定transport/version/capability策略。
- [x] 确定不公开manual CompactSession。
- [x] 列出禁止公开的内部对象。
- [ ] 实现protocol types和MiniCoreRuntime facade。
- [ ] 实现CommandSurface target architecture。
- [ ] 实现Runtime/Session snapshot和event publishers。
- [ ] 完成contract tests和首个host integration。
