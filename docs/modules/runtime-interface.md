# Runtime Interface 与公开协议架构设计

日期：2026-07-25

状态：当前权威架构（设计已冻结，生产实现待启动）

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
- `CommandId`用于协议命令correlation和in-flight去重；对Submit，它同时作为Turn创建前的process-local admission/cancel target，不是领域entity；
- Command response 在对应 command 的明确线性化点返回 typed outcome，不使用只有 `accepted: bool` 的通用 acknowledgement；
- Turn 的长期完成、Item 生命周期和 Interaction request/resolution通过 Event 发布；
- `StateEvent`与可合并/丢弃的`ProgressEvent`分离；
- subscribe使用snapshot-first实时流，第一帧是完整Snapshot，后续是当前连接内的StateEvent/ProgressEvent；
- 不公开cursor、event replay、Gap或跨restart续订；断线、背压或restart后重新subscribe并获取新Snapshot；
- Runtime scope和每个Session scope使用独立owner、Snapshot与event stream，不建立runtime-global event sequence；
- `RuntimeSnapshot`只覆盖Runtime/Agent/Session summary和loaded membership；
- `SessionSnapshot`覆盖一个loaded Session的current Turn、Items、Pending Interaction和queues；
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
- `CommandId`：public command correlation和in-flight dedup identity；Submit的CommandId同时标识领域Turn创建前的process-local admission candidate；

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

`CommandId`由调用adapter在dispatch前生成，必须使用不可预测随机值且不得复用于另一条命令。当前Runtime内原命令仍in-flight时，相同`CommandId + exact typed command`重试加入同一completion；同一in-flight CommandId携带不同command返回`CommandConflict`。CommandId不持久化，restart后旧命令不能靠它重放或恢复；调用方改用Snapshot/Query确认durable结果，并为新命令生成新ID。每个Command使用语义明确的expected revision、expected status或expected TurnId表达乐观并发。

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

`SessionDefinitionPatch`原子修改Workspace、SessionModelConfig或SessionPromptSelection，并生成新的`SessionDefinitionRevision`。修改Agent reference必须走`UpgradeAgentRevision`。

`Create` 只接受 `agent_id`：Runtime 在创建的 Agent lifecycle synchronization 内读取该 Agent 当时的 current revision，并把它作为 exact `AgentRevisionRef` 钉进 `SessionDefinition`。调用方不在 create 时报 revision——「用哪一版」由 Runtime 在此刻快照 current 决定，之后 Agent 再发布新 revision 不会改变该 Session（snapshot-current）。

`UpgradeAgentRevision.target` 为 `Option`：缺省（`None`）表示「重新钉到该 Agent 当前 current」（显式 reload 升级），是常规路径；给出 exact `AgentRevisionRef` 表示钉到指定版本（可用于钉旧版或回滚）。两种情况都在 gates 内校验 target 属于同一 AgentId、Agent 为 Enabled、target definition 存在，并原子解析为 exact ref 后写入新的 `SessionDefinitionRevision`；`latest` 本身不进入 durable `SessionDefinition`。保持exact pin让同一Session在两次upgrade之间的Agent selection、Workspace和Model配置稳定；显式Prompt resource reload仍只影响future Turn。

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
    CancelQueuedMessage {
        session_id: SessionId,
        target_command_id: CommandId,
    },
    Cancel {
        session_id: SessionId,
        target: PublicCancelTarget,
        reason: UserCancelReason,
    },
}

pub enum PublicCancelTarget {
    Submit(CommandId),
    Turn(TurnId),
}
```

语义：

- `Submit`只在Session可以admit新Turn（Idle admission decision）时使用；Session非Idle时返回`SessionBusy`，不排队跨Turn等待；Turn Running期间的用户输入应由adapter路由为`Steer`（交互式默认）或`FollowUp`；initiating UserMessage append/apply后返回`TurnStarted`；
- `Steer`只作用于expected Running Turn；成功进入该Turn的bounded `SteerQueue<TurnId>`后返回`Queued`；
- `FollowUp`进入bounded process-local FIFO；返回`Queued`；
- `CancelQueuedMessage`只删除尚在Steer/FollowUp FIFO中的目标消息；未找到统一返回typed `QueuedMessageNotQueued`，不区分从未排队与已经出队，消息不会重新入队；
- `Cancel(Submit(command_id))`允许取消该CommandId对应、尚处于排队或Starting admission的Submit；Turn创建后调用方使用`TurnId`取消；
- `Cancel(TurnId)`通过per-session target-scoped sticky`EmergencyControl`校验active Turn generation并发布cancel epoch；成功后立即返回typed`CancelAccepted`，不等待普通work lane、Tool settlement或terminal append；stale target不影响新Turn；
- Cancel current Turn清理该Turn尚未append的Steer，默认保留FollowUp，并在Finishing期间继续允许新的FollowUp进入bounded FIFO；停止全部queued work需要未来显式`StopAll`/`ClearQueuedMessages`能力；
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

`InteractionResolutionInput`必须与request family匹配：ToolApproval接受approval decision，UserQuestion接受UserAnswer；Cancelled按领域规则关闭任一family。所有identity与family校验由MiniCore执行，不能信任UI本地状态。

MiniCore拥有Interaction protocol：它创建`RequestId`并绑定Turn/Item，持久化request/resolution，处理Cancel、terminal cleanup、Unload、幂等和recovery。用户长时间无回答、暂时没有subscriber或transport断开都保持Pending，不产生默认Deny。Presentation Adapter只把UI-safe request渲染为弹窗、聊天消息、终端菜单或表单，并把用户答案提交为`InteractionCommand::Resolve`；它不能自行创建MiniCore未请求的Pending Interaction，也不能直接持有Tool waiter或SessionWriter。

结构化Interaction answer不是UserMessage，不开启新Turn。尤其是UserQuestion回答会恢复原来的ToolInvocation；若UI把答案改成Submit/UserMessage，就会错误地创建新Turn。

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

`delivery = None`时默认解析为`Submit`。交互式UI应按观察到的Session状态路由：Turn Running时默认`Steer { expected_turn_id }`，用户显式选择"排到下一轮"时使用`FollowUp`。Steer因Turn terminal返回typed stale/terminal outcome时，adapter把同一输入改发为`Submit`（开启新Turn）或提示用户；Submit因竞态返回`SessionBusy`时，adapter按新Snapshot重新路由。运行中输入的解释属于adapter层，Runtime不把Submit静默转换为Steer。

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
    SteerQueued {
        turn_id: TurnId,
    },
    FollowUpQueued,
    QueuedMessageCancelled,
    InteractionResolved,
    CancelAccepted {
        target: PublicCancelTarget,
        cancel_epoch: u64,
    },
    CommandOutput,
    NoChange,
}
```

Command response不是完整业务完成流：

- `TurnStarted`只表示领域Turn已由initiating UserMessage append创建；
- Turn最终`Completed | Interrupted | Failed`由StateEvent发布；
- `CancelAccepted`只表示target/generation仍可取消且sticky cancel epoch已经发布；它与initiating/final append reservation first-wins，accepted后对应Started/Completed commit不得再赢；Tool清理和Turn terminal通过后续StateEvent/Snapshot观察；
- `SteerQueued`和`FollowUpQueued`只表示当前Runtime的对应SessionIngress lane已接收，不承诺crash-safe delivery；restart后未append的消息消失，host以新Snapshot为准；真正append通过普通UserMessage/Turn StateEvent观察；
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
| Unload Session | LifecycleControl完成grace/fail-closed drain，writer关闭并从loaded map移除 |
| Submit | initiating UserMessage append/apply |
| Steer Queued | target Turn的SteerQueue admission |
| FollowUp | FollowUpQueue admission |
| CancelQueuedMessage | target仍在任一FIFO时remove；否则返回QueuedMessageNotQueued |
| Resolve Interaction | InteractionResolved append/apply |
| Cancel Submit(CommandId) | target/generation仍可取消且sticky cancel epoch发布；initiating append reservation先赢时返回typed transition error；Submit的最终Rejected(Cancelled)由原Submit response表达 |
| Cancel Turn | target/generation仍可取消且sticky cancel epoch发布；final append reservation先赢时返回typed terminal/transition error；最终TurnInterrupted由StateEvent/Snapshot表达 |
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
IngressLaneFull
QueuedMessageNotQueued
ExpectedTurnMismatch
TurnNotRunning
TurnCancelling
TurnTerminal
InteractionNotFound
InteractionAlreadyResolved
InteractionFamilyMismatch
InvalidForkAnchor
Unauthorized
Unavailable
DurableStateCorrupt
RuntimeClosing
```

外部调用方不能解析自然语言message决定retry。`RetryAdvice`使用typed值，例如`DoNotRetry | RefreshAndRetry | RetryWithBackoff | UserActionRequired`。

`IngressLaneFull`必须携带safe lane kind（TurnAdmission、Steer、FollowUp、InteractionControl或ToolControl）；EmergencyControl、LifecycleControl和SnapshotMailbox不返回该错误。

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
ListItems { turn_id }
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

History node使用Turn/Item/message语义，不向普通UI暴露raw SessionEntryDraft、writer internals、ToolRoundCompleted内部event或writer current EntryId。`GetHistoryTree`首版按一个明确`SessionHistoryRevision`返回完整compact branch topology，不内联Turn Item bodies，也不分页；它服务fork/navigation拓扑，不代替聊天timeline读取。

MVP只要求真正可能持续增长的`ListSessions`、`ListTurns`和大型catalog query支持分页。实际场景是长期Session包含数千个Turn，UI首次加载最近一段历史、向上滚动时继续读取；`GetTurn`、turn-scoped `ListItems`和`SessionSnapshot.active_items`首版完整返回，不为单个active Turn增加分页。分页Cursor保持opaque并绑定query family、filter、明确sort和revision；调用方不能把不同revision的page任意拼接。

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
    pub revision: Option<QueryRevision>,
    pub data: QueryResult,
}
```

Query不发布Event、不创建Turn、不加载完整SessionExecutor来读取持久化目录，也不消费Steer/FollowUp queue。Query的多个独立调用不承诺形成跨调用原子Snapshot；需要完整恢复读模型时使用Snapshot或snapshot-first subscription。

## Snapshot

Snapshot用于UI初始化、显式状态读取和subscriber重建。

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

pub struct SessionQueueView {
    pub pending_submit_count: usize,
    pub current_turn_steer_count: usize,
    pub follow_up_count: usize,
    pub accepting_input: bool,
}
```

长期Turn历史通过Query按需读取。Snapshot只携带当前loaded Session的live observer baseline；`active_items`完整包含current Turn的committed Items，并按selected path entry顺序与assistant Reasoning/Text/ToolCall content顺序排列。它不是durable checkpoint，也不用于恢复旧Model/Tool waiter。

`SessionQueueView`只暴露public input admission状态；InteractionControl、ToolControl、emergency/lifecycle waiter和SnapshotMailbox深度属于internal diagnostics，不进入普通公开协议。`pending_submit_count`是尚未被仲裁的admission信箱瞬时计数（正常为0），不表示存在跨Turn排队的Submit通道。

Cancel被Executor接受后，`SessionExecutionView`必须立即反映`Finishing`并发布`session_execution_changed`。当execution为Finishing时，host优先显示Stopping/Finishing；`CurrentTurnView.phase`保留Cancel前最后工作位置供diagnostic，不建立`TurnExecutionPhase::Cancelling`。Finishing期间host可以继续发送FollowUp并收到`FollowUpQueued`，普通Submit仍返回SessionBusy。

### Snapshot Consistency

Runtime与每个Session使用独立owner和Snapshot，不存在跨scope可比较的全局sequence。

`SessionSnapshot`从对应SessionExecutor的latest-wins/coalesced immutable published view读取，不进入mutation/control lane，也不承诺与不同SessionIngress lane形成全局FIFO。`RuntimeSnapshot`由Runtime owner捕获Agent/Session membership和runtime projection，不等待所有SessionExecutor parked，也不构造all-loaded stop-the-world barrier。

单独调用`snapshot()`只读取调用时的当前view。用于持续观察时，host调用`subscribe(scope)`：owner必须在同一publication synchronization内注册subscriber并捕获初始Snapshot，EventStream第一帧返回该Snapshot，随后只发送该点之后的实时事件。禁止用非原子的“先snapshot再subscribe”或“先subscribe再snapshot”替代。

断线、subscriber背压或publisher关闭后，旧stream直接结束；host重新subscribe并从新的首帧Snapshot恢复，不重放缺失事件。Runtime process restart时先从JSONL replay并执行conservative recovery，使旧unfinished Turn terminalize并让Session进入Idle/Ready/Unavailable；之后的新Snapshot只反映该恢复结果，不承诺从旧执行现场继续。

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

pub struct SubscriptionRequest {
    pub scope: SubscriptionScope,
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
    Snapshot(SnapshotResponse),
    State(StateEvent),
    Progress(ProgressEvent),
    Closed(SubscriptionClosed),
}
```

### StateEvent

```rust
pub struct StateEvent {
    pub event_id: EventId,
    pub timestamp: Timestamp,
    pub command_id: Option<CommandId>,
    pub route: EventRoute,
    pub msg: StateEventMsg,
}
```

StateEvent本身始终是非durable observer record，不因payload来源不同而成为第二日志：

| 来源 | 例子 | restart后的恢复 |
| --- | --- | --- |
| committed-derived | Agent/Session definition、Turn/Item terminal、Interaction request/resolution | 从durable catalog或SessionStorage projection重建，并出现在新Snapshot/Query中 |
| process-local | load/readiness、execution/phase、queue、settled、diagnostics | 只描述当前Runtime状态；restart后可以消失、重置或由新状态替代 |

StateEvent规则：

- subscription第一帧必须是与scope匹配的完整Snapshot；
- 同一subscription lifetime内，StateEvent按publisher发送顺序交付；
- subscriber queue无法继续、transport断开或publisher restart时发送Closed或直接终止stream，调用方重新subscribe并从新Snapshot恢复；
- 不缓存StateEvent用于公开replay，也不接受caller-provided offset；
- durable conversation fact必须从append/apply后的CommittedSessionEntry派生；
- process-local load/readiness/execution/phase/queue事实必须能从当前Runtime的对应Snapshot读取，但不承诺跨restart恢复；
- payload包含完整final view，能够校正之前丢失的ProgressEvent。
- Cancel/PrepareForUnload清理process-local Steer或FollowUp时，`queue_updated`携带被移除的CommandId和typed reason；它只说明队列事实变化，不把未append消息伪造成durable UserMessage。

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

ProgressEvent不参与可靠状态恢复，可以合并或丢弃。典型内容：

```text
agent_message_started
agent_message_delta
reasoning_started
reasoning_delta
tool_output_delta
model_retry_scheduled
model_attempt_progress
```

规则：

- 有stream progress的AgentMessage/Reasoning在首个content update时分配稳定`ItemId`；started、全部delta和最终`item_completed`使用同一ItemId；
- message/reasoning started属于AgentRun ProgressEvent，不创建durable `ItemStatus::Started`；CompactionSummary不创建StreamingItem或ItemId；
- Host漏掉started时可以用首个delta创建临时Item view；漏掉全部progress或provider为non-streaming时直接使用final `item_completed`；
- 可以按SessionId/TurnId/ItemId合并连续delta；
- queue满时可以丢弃中间progress；
- progress缺失不影响StateEvent或Snapshot正确性；
- `item_completed`只在final candidate成功append/apply后发布，携带完整final Item view；append失败不能发布completed；
- Cancel/failure丢弃未commit的streaming state；logical `model_retry_scheduled`要求Host清除上一Model operation的临时view，若progress丢失则由Turn terminal或新Snapshot最终校正；
- Host收到同ItemId的`item_completed`或Turn terminal后忽略迟到started/delta；
- progress不写SessionStorage，不成为conversation truth；
- progress publisher失败不影响Turn执行或terminal。

### Turn And Item Ordering

不公开`DisplaySequence`、ordinal或可排序ID。公开排序由现有有序集合和new-Item StateEvent创建顺序表达：

```text
Turn order
→ selected history path上的initiating UserMessage顺序

Item order
→ selected path entry顺序
→ 同一assistant entry内的Reasoning/Text/ToolCall content顺序
```

`ItemId`、`EntryId`、`ToolCallId`、timestamp、Tool completion time和ProgressEvent arrival都不能用于排序。`SessionSnapshot.active_items`和turn-scoped `ListItems`按canonical Item order返回；Host必须保留Vec顺序。

新committed Item的StateEvent按canonical创建顺序发布并追加到durable Item list。ToolInvocation的Completed/Abandoned事件和Tool progress都只按`ItemId`更新原位置，不移动Item；因此Tool B可以先结束并先通过process-local activity显示完成，authoritative terminal view稍后校正，但展示位置始终保持call order A、B。

AgentMessage/Reasoning progress只属于独立provisional view，first-seen顺序没有durable意义。收到matching new-Item committed StateEvent时Host删除provisional view，并按该创建事件顺序加入durable list；Snapshot则整体替换durable list并清空provisional view。未知Item的terminal update或任何需要插入既有durable Item之前的事件表示reducer失步，Host应重新subscribe获取Snapshot而不是猜测顺序。

### Interaction Event

`interaction_requested`必须携带UI-safe request：

```rust
pub struct InteractionView {
    pub request_id: RequestId,
    pub turn_id: TurnId,
    pub item_id: ItemId,
    pub request: InteractionRequestView,
    pub state: InteractionStateView,
}

pub enum InteractionRequestView {
    ToolApproval(/* UI-safe approval fields */),
    UserQuestion(/* UI-safe question fields */),
}
```

prepared Tool args、executor handle、sandbox internals和credential不进入event。Host回答时必须回传SessionId、expected TurnId、ItemId、RequestId和resolution key。UI可以执行本地required-field/choice校验，但MiniCore仍要校验resolution family、identity和first-wins状态。

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

- 读取完整compact history tree以及Turn/Item read model；长期Session的`ListTurns`使用分页，`GetTurn`和turn-scoped `ListItems`首版完整返回；
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
- unload先设置`PrepareForUnload` lifecycle signal：停止admission、清理queued input，在有限grace期后fail-closed cancel，再移除loaded owner；
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
→ per-session SessionIngress.TurnAdmissionQueue
→ SessionExecutor reserves candidate Turn
→ WorkspaceResolver.resolve()
→ ModelGateway.resolve_for_turn()
→ PromptService.current_view()
→ SkillService.current_view()
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
→ ModelGateway private ProviderAdapter（production可使用RigProviderAdapter，仅处理provider attempt）
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

### User Question

```text
ModelCallResult contains built-in ask-user ToolCall
→ SessionExecutor creates ToolInvocation ItemId
→ ToolExecutionControl.request_user_question
→ append/apply InteractionRequested(UserQuestion)
→ session StateEvent::InteractionRequested with UI-safe InteractionView
→ Presentation Adapter展示并收集答案
→ UI dispatch InteractionCommand::Resolve(UserAnswer)
→ append/apply InteractionResolved
→ session StateEvent::InteractionResolved
→ wake original Tool future
→ append/apply PreExecution truthful Tool result
→ append/apply tool_round_completed
→ same Turn next PromptSet.assemble
```

Pending期间`TurnStatus`和`SessionExecutionState`仍为Running，`TurnExecutionPhase = WaitingForUserInput`。当前Turn的逻辑执行暂停，但对应SessionExecutor继续处理Resolve/Cancel/Unload/Snapshot；其他Session的Executor不受影响。等待期间不预留file mutation ticket，也不持有Workspace commit authorization，elapsed time不会自动关闭Interaction。

## Transport 与 Adapter

生产优先级：

1. in-process Rust调用；
2. Tauri backend在可信进程内调用同一interface；
3. 测试使用in-memory runtime和synthetic provider/tool adapters。

JSON-RPC、stdio或WebSocket adapter出现第二个真实transport需求后实现。

Transport Adapter只负责：

- serialization/deserialization；
- transport request id correlation；
- connection lifecycle；
- frame size、backpressure和authentication；
- protocol initialize/version negotiation；
- EventStream到transport notification映射。

Transport Adapter不负责：

- Session selection；
- command parse/authorization；
- Session state；
- Tool approval policy；
- provider retry；
- storage truth。

Presentation Adapter与Transport Adapter可以由同一个host实现，但职责不同：Presentation Adapter决定modal、聊天卡片、终端菜单、表单文案与本地交互；Transport Adapter只传输Runtime已定义的request/resolution。两者都不拥有MiniCore Interaction truth。

Transport request id不能替代CommandId、TurnId或RequestId。

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
- PromptSet完整System sections和前置User context；
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
SessionIngress及其内部lane
TurnControlGate / LifecycleControl / EmergencyControl
RunningOperation
OperationResult
execution_version
AgentLoop
TurnExecutionContext
PromptService / PromptSet
ToolService / ToolSet
SkillService / SkillView / LoadedSkill
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
  scope不支持、Session未loaded、publisher unavailable

StateEvent
  已接受异步工作的后续业务失败，例如TurnFailed
```

Model、Tool和Storage内部错误必须归约成typed、redacted public failure。调用方不能解析provider字符串决定retry。

## Test Strategy

Public interface是 contract test surface。

### Command Tests

- Agent/Session revision CAS；
- Submit只在UserMessage append/apply后返回TurnStarted；
- duplicate in-flight Submit使用相同CommandId加入同一completion，不创建第二个candidate；
- 相同CommandId携带不同command返回CommandConflict；
- Cancel(Submit CommandId)关闭排队或Starting admission，target退休或restart后返回NotFound且不影响future Turn；
- Steer expected TurnId与per-Turn FIFO Queued；CancelQueuedMessage只remove一条/NotQueued；
- FollowUp bounded queue和restart loss；
- 普通work lane满时Cancel仍可进入EmergencyControl并触发取消；
- valid Cancel立即返回`CancelAccepted`且不等待Tool settlement；duplicate返回同一target/epoch；
- Cancel与initiating/final append reservation first-wins，不能accepted后再提交Started/Completed；
- Cancel清理current Turn Steer但保留FollowUp，Finishing期间新FollowUp仍可Queued；
- CancelAccepted后execution snapshot/event进入Finishing，最终TurnInterrupted另行发布；
- stale Cancel TurnId/generation不取消新的active Turn；
- Unload grace deadline到期后Cancel active Turn并以Cancelled关闭Interaction；
- Interaction长时间无回答或subscriber断开时保持Pending，不产生默认Deny；
- Interaction first committed resolution wins；
- Fork Before/After Item anchor；
- `/model`和`/thinking`生成new SessionDefinitionRevision；
- `/compact`不在catalog。

### Query/Snapshot Tests

- Query只读且不发布Event；
- session list不加载SessionExecutor；
- GetHistoryTree在单一SessionHistoryRevision返回完整compact branch topology，不内联Item bodies且不分页；
- history tree不暴露internal entry/event；
- 长期Session的ListTurns分页可用于首次加载最近历史和向上滚动，cursor绑定sort与revision；
- GetTurn、turn-scoped ListItems和active_items首版不分页；
- SessionSnapshot读取immutable published view，active_items保持canonical Item顺序；
- RuntimeSnapshot不等待所有SessionExecutor；
- pending Interaction可从SessionSnapshot恢复；
- snapshot-first subscription的首帧Snapshot与后续事件无缺口、无旧事件回放。

### Event Tests

- Runtime与每个Session subscription彼此独立；
- 每条stream首帧是scope匹配的Snapshot；
- 当前subscription内StateEvent保持发送顺序，不区分committed-derived与process-local来源；
- subscriber背压、disconnect和restart关闭stream，重新subscribe返回新Snapshot；
- restart后durable-derived状态从projection重建，queued Steer/FollowUp和旧phase等process-local状态不恢复；
- ProgressEvent可以合并/丢弃；
- message/reasoning started、delta和completed使用稳定ItemId，丢失started/delta仍可由completed校正；
- append失败不发布item_completed；logical retry、Turn terminal或新Snapshot清理临时Item view，completed/terminal后忽略迟到progress；
- logical model_retry_scheduled丢失时不影响最终Snapshot/terminal校正；
- final Item event携带完整view；
- assistant content创建事件按Reasoning/Text/ToolCall顺序发布；
- parallel Tool逆序完成只更新各自原位置，不改变call order；
- Snapshot替换有序durable Items并清空provisional Items；
- Runtime restart从JSONL replay和conservative recovery开始，不把Snapshot当作execution checkpoint；
- Interaction request-after-append和resolution-before-resume；
- elapsed time和subscriber缺失不产生Interaction resolution；
- UserQuestion event只携带UI-safe view，UI提交UserAnswer后恢复同一Turn而不是创建UserMessage；
- Pending UserQuestion可由SessionSnapshot重建展示，Session A等待不影响Session B事件推进；
- Turn只有一个terminal event；
- subscriber buffer不足时关闭stream，不做event replay。

### Security Tests

- catalog/query/snapshot/event不包含credential；
- renderer不能提交raw internal command；
- renderer不能直接持有Tool waiter、SessionWriter或伪造MiniCore未请求的Pending Interaction；
- stale catalog selection重新resolve；
- cross-sessionItem/RequestId拒绝；
- Workspace restriction通过Runtime path生效。

## 方案比较

### 全局 Event Sequence + All-Loaded RuntimeSnapshot

优点：所有event有单一total order，reducer概念简单。

缺点：每次完整snapshot需要协调所有SessionExecutor；一个慢Session拖住全Runtime；filtered subscriber仍要处理无关event；多Session之间通常没有业务因果关系。

结论：不采用。Runtime与Session使用独立Snapshot和snapshot-first实时流，不建立全局或per-scope公开cursor。

### 一个长请求持有完整Turn

优点：CLI调用简单，response就是terminal result。

缺点：不利于后台多Session、Steer、Interaction、断线恢复和独立subscriber；transport request lifetime与领域Turn lifetime耦合。

结论：不采用。Command在短线性化点返回，Turn completion走Event。

### Generic Accepted Bool

优点：dispatch实现简单。

缺点：调用方无法知道revision、TurnId、Queued或Interaction resolution outcome；需要额外event猜测command结果。

结论：不采用。返回typed CommandOutcome。

### CommandSurface 由每个UI实现

优点：UI可以快速定制。

缺点：slash解析、catalog revision、权限、动态候选和handler语义漂移。

结论：不采用。Runtime拥有CommandSurface，UI只渲染和提交。

### Event 同时作为 Durable Conversation Log

优点：单一stream看似可以同时驱动模型与UI。

缺点：streaming/progress、provider retry和UI状态会污染Session durable truth；ToolRoundCompleted模型可见性无法安全表达。

结论：不采用。SessionStorage是durable truth；committed-derived StateEvent从durable projection派生，process-local StateEvent从当前Runtime/Executor view派生，两者都不成为event log。

### 公开 Manual CompactSession

优点：用户可主动降低上下文占用。

缺点：需要standalone Session maintenance state、admission/queue/cancel和模型选择规则；Compaction只在active Turn NeedModel安全点定义完整语义。

结论：不采用。未来出现真实需求后独立设计。

## 实现顺序

1. 冻结public serde naming和protocol crate layout；
2. 实现Agent/Session durable command/query owner；
3. 实现MiniCoreRuntime dispatch routing；
4. 实现Runtime snapshot-first publisher和RuntimeSnapshot；
5. 实现per-session snapshot-first publisher、SessionSnapshot和原子subscriber注册；
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
- [x] 确定Runtime与per-session snapshot-first实时流，不公开cursor/replay。
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
