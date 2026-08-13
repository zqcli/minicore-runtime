# Runtime Interface 与公开协议架构设计

日期：2026-07-31

状态：当前权威架构（ADR 0136/0137；M5 durable foundation与replay/Recorder-backed hydration、Workspace resolver/Snapshot及Runtime-owned residency foundation已实现；minimal public dispatch/query/snapshot/subscribe已接通Agent Create/Enable/Disable/Delete/UpdateDefinition/UpdateMetadata与Session Create/Load/Submit/Unload/Fork/Archive/Unarchive/Delete/UpdateDefinition/UpgradeAgentRevision/ReloadWorkspace/UpdateMetadata、typed ResolveInteraction/NoChange、durable `ListAgents`/`ListSessions`分页、`GetSessionForkProvenance`、snapshot-first Runtime/Session subscription、Runtime Agent/Session membership/lifecycle/definition/metadata StateEvent、loaded Session metadata/definition/workspace-reload StateEvent、loaded Ready+Idle与Workspace/Prompt Unavailable Snapshot及Turn Completed/Failed Event；M9 Starting/Running/Finishing observation、current Turn/active Items/Pending Interaction安全摘要已接通；Session usage、Degraded recording与bounded diagnostics也已接通；selected-V1 Agent/Session metadata mutation、ordinary Session definition CAS、Agent revision upgrade、Ready-state与Unavailable恢复的reload workspace、Progress/Closed及SessionDefinitionUpdated codec已关闭public manifest pending；Workspace/Prompt Unavailable loaded readiness与ReloadWorkspace恢复已实现；Agent readiness fan-out亦已实现（`SetStatus/Delete` durable Updated后按同一owner timestamp经residency per-Session gate逐个fan-out `set_agent_availability`，Idle立即应用、非Idle保存最新pending并在回Idle后应用，仅public readiness真实变化发布Session-scope `session_readiness_changed`，Agent Disabled/Deleted的Load仍返回Loaded并投影AgentUnavailable，Enable恢复底层Ready或原resource Unavailable，active Turn不变且future admission拒绝）；ModelUnavailable load/definition projection亦已实现（`SessionExecutorSnapshot`新增独立`model_available: bool`事实并收窄重命名`resource_unavailable`为`workspace_unavailable`，readiness只在Idle按固定优先级为AgentUnavailable→workspace cause→ModelUnavailable→Ready（非Idle执行始终投影Ready；facts是future-only，new Unavailable只在回Idle后显现）；Load用现有`resolve_for_turn`按captured definition.model同步分类model_available，普通model incompatibility→false且Load仍Loaded而catalog owner/source/definition internal→现有internal load路径，任何install新definition的publication install前按当前catalog计算新definition的model_available并与definition一起安装，ReloadWorkspace保留当前事实，true Workspace publication只清workspace cause，DefinitionUpdated/WorkspaceReloaded event snapshot只在Idle publication携带derived readiness——Running等非Idle执行时保持Ready、new facts经terminal/回Idle显现）；selected PromptUnavailable load/definition projection亦已实现（`SessionExecutorSnapshot`新增独立`prompt_available: bool`事实，readiness只在Idle按固定优先级为AgentUnavailable→workspace cause→selected prompt unavailable→PromptUnavailable→ModelUnavailable→Ready（非Idle执行始终投影Ready；facts是future-only，new Unavailable只在回Idle后显现）；Load与任何install新definition的publication在install前经`read_agent_definition`读exact retained Agent revision并复用`for_turn` selection阶段验证exact Agent+Session Prompt selection，missing/wrong role/duplicate resolved key→PromptUnavailable而非Workspace cause，Agent read的Closing→Load Closing、其余Agent read失败与owner/identity mismatch→internal路径，ReloadWorkspace保留当前事实）；shared-resource reload recovery/fanout亦已实现（`ReloadSharedResources`并行build candidates并预计算后经residency per-Session gate fan-out new Prompt/Model roots至全部loaded executors，非Idle合并单一pending availability composite在terminal/admission failure后应用，随后一次原子替换Runtime root pair并发布`shared_resources_reloaded`）；active-Turn graceful Unload亦已实现（`MiniCoreRuntimeConfig::with_unload_grace` default 30s/≤5min验证；public Unload经runtime publication gate与residency per-Session gate执行prepare→close→remove_exact，executor先同步关admission gate并经unbounded emergency lane接受`PrepareUnloadRequest`，grace内active admission/Turn自然完成，deadline到期对exact current emergency target signal `PrepareForUnload`（sticky first-wins，更早Cancel/SecurityRevoked保留原reason）并cancel其cancellation token、以`SessionUnloaded` settle pending Interaction、不直接drop task；Starting Submit在Input未live apply时经internal `SessionSubmitError::PrepareForUnload`公开映射`SessionNotLoaded`而非`SubmitCancelled`，Input先赢仍`TurnStarted`随后同一Turn `Interrupted(PrepareForUnload)`；registry shutdown先广播begin_prepare使grace并行再逐个await waiter再close；不新增queue_updated event，manifest现为144项）；host security Workspace authority invalidation亦已实现（`MiniCoreRuntime::invalidate_session_workspace_authority(session_id)`为public host-only非wire async seam，返回redacted `SessionWorkspaceInvalidationError{RuntimeClosing,SessionNotLoaded,InternalDispatchUnavailable}`，host已先发布current hard restriction fact、Runtime只驱动loaded executor的signal+Workspace re-resolve且不改durable definition/revision/metadata/conversation；route不获取runtime_publication semaphore、不等待普通work lane，经residency loaded map直接clone executor调用out-of-band API（先同步close admission gate再经unbounded emergency lane发送），采样single SystemClock timestamp、无CommandId；missing loaded executor或executor普通Closing且registry未closing（per-Session Unload/old exact executor race）→SessionNotLoaded，仅registry/runtime closing→RuntimeClosing，fatal→Internal；active admission/Turn对exact current emergency target发sticky `SecurityRevoked` first-wins（更早Cancel/PrepareUnload保留原reason、仅形成Turn投影Finishing、pre-Input Starting legal、pending Interaction按SecurityRevoked truthful settlement；即使active publication在飞也立即signal——publication不屏蔽security signal），仅无admission/Turn而publication在飞也立即进入`Preparing`（drop旧WorkspaceSnapshot、发布唯一一次`ReadinessChanged(None)`、publication不取消不阻塞）但recovery worker仍等其settlement并以post-publication exact definition启动（settle后幂等re-enter不重复start event、settled snapshot不重新安装），只要Idle且无active admission/Turn即进入`Preparing`（active publication不阻塞Preparing entry、只等Turn/admission settle；`workspace_preparing`最高readiness优先级、必须Idle+workspace None+空public queues/accepting false），recovery worker单独等待publication settle后才spawn（全空Idle立即spawn，owner-tracked；重复invalidation join同一state）；recovery复用ReloadWorkspace resolve/capture/revalidate/finish（最小shared helper，install前验证Arc ptr_eq与snapshot SessionId/revision、不调用DurableState），非internal resolver失败（含AuthorityDenied）→WorkspaceUnavailable、Workspace Prompt SourceDiscovery/ContentLoad/DuplicateKey→PromptUnavailable、Closing→Closing、shape/task mismatch→fatal Internal；start/finish各发布一次`session_readiness_changed`（command_id None）、不发WorkspaceReloaded event、No Runtime event；recovery pending禁止FollowUp pop/start、新Submit因gate close失败（Preparing→`SessionNotReady`+RetryWithBackoff）；close/fatal/reap settle security waiters exactly once（closing→Closing、fatal→Internal）、worker owner-tracked并reap；`SessionExecutorEvent::ReadinessChanged.command_id`改`Option<CommandId>`（Agent/shared reload传Some、security传None、Runtime EventStream直接复用Option）。manifest现为144项，wire `Preparing`由并行worker激活、host security Preparing/active Turn duplicate recovery fixtures/tests已补齐，scenario/fixture closure已实现、统一质量门禁已通过）；RuntimeDependencyUnavailable loaded readiness与probe recovery亦已实现（唯一producer为loaded Turn admission读pinned historical AgentRevisionRef时的transient StorageUnavailable、owner-tracked无TurnId probe与Submit re-arm恢复），full recovery scenario/fixture closure已实现并通过统一质量门禁；完整cross-platform native matrix acceptance已通过（全部七个`platform_m5_0`坐标均有对应的production行为与测试覆盖；GitHub Actions run 31433810296四个job全部通过：Ubuntu Rust stable、Ubuntu Rust 1.85.0、cargo test macos-latest、cargo test windows-latest）））；production `ask_user`/`read_file`/`list_directory`/`write_file`/`fetch_url` builtins亦已实现（五个public host opt-ins均default-off/idempotent且相互独立，`with_fetch_url_origin(...)`独立安装authority；`open`把四个ask/filesystem bool与materialized fetch authority Option冻结为32种closed selection，固定顺序`ask_user → read_file → list_directory → write_file → fetch_url`；每次Turn admission对exact captured Workspace snapshot materialize enabled filesystem routes，read/list-only选择ReadOnly ceiling，write opt-in选择ReadWrite ceiling但requested ReadOnly绝不提升，任一filesystem opt-in下host invalidation先经owner-held `WorkspaceFilesystemAccessControl`永久revoke，future read/list/write共同Denied；`write_file`另实现capability physical target、same-Session FIFO与permit-through-Settling；`fetch_url`实现exact HTTPS origin + pinned addresses、无ambient DNS/redirect/retry/proxy/compression、bounded safe text与owner-contained cancellation））

> v0.1前端闭环细化：`c99ccf7`已增加[ADR 0148](../adr/0148-v0-1-session-transcript-is-a-library-only-read-seam.md)冻结的library-only `MiniCoreRuntime::session_transcript`。它从loaded Session的canonical selected history分页恢复基础User/Assistant文本，并明确把`GetHistoryTree/ListTurns/GetTurn/ListItems`、unloaded direct history read与Wire transcript route冻结为post-MVP。Public Wire V1 manifest仍为144 active / 0 pending，Store V1与Conversation JSONL V1均未变化。

## 目的

本文定义 MiniCore 的公开 Runtime interface，回答：

- 外部 CLI、TUI、Tauri host 或其他 adapter 如何调用 MiniCore；
- Command、Query、Snapshot 和 Event 各自负责什么；
- Agent、Session、Turn、Item 和 Interaction 如何进入公开 payload；
- Command 何时返回、异步业务完成如何通知；
- v0.1基础聊天transcript如何通过library-only Runtime seam恢复，以及完整message tree/history生态为何后置；
- streaming progress 与可靠状态变化如何分离；
- 多 loaded Session 如何订阅、恢复和避免全 Runtime snapshot barrier；
- slash command 和 GUI command palette 如何共享 CommandSurface；
- transport、protocol version、capability、redaction 和兼容策略；
- 哪些内部对象永远不能进入公开协议。

相关权威文档：

- [MiniCore 领域模型](../architecture.md)
- [Agent 与 Session 生命周期](agent-session-lifecycle.md)
- [Turn、Item 与 Interaction](turn-item-interaction.md)
- [Conversation Recording 与 Replay](conversation-storage.md)
- [Session Execution](session-execution.md)
- [ModelGateway](model-gateway.md)
- [Compaction](compaction.md)

## 非目标

本文不定义：

- CLI、TUI、Vue、Tauri command 或 widget 的具体实现；
- JSON-RPC、WebSocket、Tauri IPC 的完整 wire schema；
- 独立 daemon、跨进程长期 event replay 或 multi-client authorization；
- provider、Rig、Tool executor、PromptSet 或 Conversation Storage 的公开调用入口；
- UI selected Session、输入框草稿、滚动位置、窗口布局或折叠状态；
- standalone/manual `CompactSession`；
- Extension / Plugin 协议。

## 决策摘要

- `MiniCoreRuntime` 是外部宿主接触 MiniCore 的唯一顶层门面；
- Wire-compatible transport interface固定为`dispatch`、`query`、`snapshot`和`subscribe`四类能力；Rust embedding另有少量明确标注的host-only/library-only seams；
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
- SessionRecorder保存best-effort recorded entry tree；v0.1通过library-only `session_transcript`暴露loaded selected history中的基础User/Assistant文本，通过Snapshot/Event暴露current live read model，通过Fork command创建新Session branch；
- 同一 Session 内不提供原地 checkout/navigation mutation；
- CommandSurface 是 MiniCoreRuntime 内部无状态命令解释模块；slash command 只是 command text 的一种语法；
- 所有改变 Runtime 事实的 UI 操作都经过 MiniCoreRuntime；纯 UI 状态留在 adapter；
- 公开 interface 是 in-process Rust interface 和 transport-neutral serde types；具体 transport 使用薄 adapter；
- host构造MiniCore时显式注入一个clone的`tokio::runtime::Handle`，不使用ambient `Handle::current()`；这不是第五个wire entry point。

## 同类产品取舍

| 产品 | 公开形状 | MiniCore 取舍 |
| --- | --- | --- |
| Codex App Server | bidirectional JSON-RPC；Thread → Turn → Item；request返回identity，notification发布后续状态；approval使用server request | 采用领域层级、短请求加事件流、显式Turn控制；不复制其process-global workspace和大量transport method |
| Agent Client Protocol | session/prompt长请求；session/update通知；client反向处理permission和filesystem | 采用typed Interaction和capability思想；不让一个长请求持有整个Turn完成生命周期 |
| Claude Agent SDK | long-lived Query对象、async message iterator、control methods和callback | 采用嵌入式易用性；不把callback或SDK对象句柄固化为wire contract |
| LangGraph Agent Server | Thread/Run资源、durable task queue、checkpoint、SSE join/cancel | 独立daemon出现后再评估Run resource和durable replay；不支付分布式队列成本 |
| OpenHands | SDK与Agent Server分层；append-only events同时服务memory和integration | 采用SDK/transport分层；live observer state与best-effort Session recording分离 |

## 顶层 Ownership

```text
External host
└─ MiniCoreRuntime public interface
   ├─ dispatch(CommandRequest)
   ├─ query(RuntimeQuery)
   ├─ snapshot(SnapshotRequest)
   ├─ subscribe(SubscriptionRequest)
   ├─ session_transcript(SessionId, PageRequest) [library-only]
   ├─ invalidate_session_workspace_authority(SessionId) [host-only]
   └─ shutdown() [host-only]

MiniCoreRuntime private implementation
├─ DurableStateActor / immutable Agent / Session durable catalog
├─ LoadedSessionExecutors
├─ CommandManager
├─ PromptService
├─ ToolService
├─ SkillService
├─ ModelGateway
├─ WorkspaceResolver / WorkspaceAuthority adapters
├─ DurableState / ConversationStorage / SessionRecorder
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

四个Wire-compatible entry families是同一个深模块的transport-neutral interface。它们共享identity、error、view、revision和redaction类型，但职责不重叠。Rust embedding还可以调用ADR 0148的library-only transcript read与host-only Workspace invalidation/lifecycle seams；这些方法不是新的Wire V1 route。Host-only lifecycle API固定为：

```rust
impl MiniCoreRuntime {
    pub async fn open(
        config: MiniCoreRuntimeConfig,
        handle: tokio::runtime::Handle,
    ) -> Result<Self, RuntimeInitializationError>;

    pub async fn shutdown(&self);

    pub async fn session_transcript(
        &self,
        session_id: SessionId,
        page: PageRequest,
    ) -> Result<Page<SessionTranscriptItem>, QueryError>;
}

pub enum RuntimeInitializationError {
    InvalidConfiguration,
    RuntimeDependencyUnavailable,
    StoreInUse,
    UnsupportedStoreFormat,
    DurableStateCorrupt,
    DurableStateTooLarge,
    StorageUnavailable,
}
```

`MiniCoreRuntimeConfig`包含host配置的dedicated durable root、Runtime-global `CompactionSettings`与现有runtime options；它不是wire DTO。五个closed production Tool opt-ins（`with_ask_user_tool()`、`with_read_file_tool()`、`with_list_directory_tool()`、`with_write_file_tool()`与`with_fetch_url_tool()`）均default-off、idempotent、相互独立；`with_fetch_url_origin(FetchUrlOrigin)`独立安装authority而不披露Tool。`open`先验证selected `fetch_url` authority（selected但zero origins或duplicate canonical origin→`InvalidConfiguration`，client build→`RuntimeDependencyUnavailable`；tool off时origins不materialize），再把四个ask/filesystem bool与materialized fetch authority Option冻结为单一production Tool config：32种selection，固定顺序`ask_user → read_file → list_directory → write_file → fetch_url`，内部Option是唯一fetch selection事实源。read/list-only选择ReadOnly Workspace authority ceiling；write opt-in存在时选择ReadWrite ceiling，但requested-access intersection仍权威，requested ReadOnly root绝不被提升。任一filesystem opt-in安装同一个per-Session永久filesystem revocation control，host invalidation后read/write共同Denied；network authority是Runtime-wide immutable exact origin/address集合，不受Workspace invalidation影响（见[Tools](tools.md#production-fetch_url-builtin)、[ADR 0146](../adr/0146-production-write-file-binds-capability-targets-to-session-fifo.md)及[ADR 0147](../adr/0147-production-fetch-url-pins-exact-https-origins-to-host-addresses.md)）。`CompactionSettings`默认启用，并可通过`with_compaction_settings(...)`替换；`open`在启动task或打开storage前验证summary min/max，失败返回redacted `InvalidConfiguration`。`open`只使用显式传入的cloned Handle，不调用`Handle::current()`；owner-tracked timer probe把missing runtime/time driver panic或join failure映射为`RuntimeDependencyUnavailable`，DurableState open failures保持一对一分类。`RuntimeInitializationError`的`Debug`/`Display`同样redacted，不暴露path、origin/address、OS source或durable bytes。`shutdown`是idempotent host-only non-wire seam，不是第五个protocol entry point。Facade `Drop`只发best-effort Closing signal且不阻塞；host必须await shutdown才能观察全部join、root lease release与Closed。

`dispatch()`的外层`Err(RuntimeDispatchError)`通常只表示请求无法进入Runtime/dedup completion owner；一旦进入，领域成功、typed rejection和user pre-Turn cancellation都通过`Ok(CommandResponse { command_id, completion })`返回。 The sole post-admission integrity-fatal outer result is the existing `RuntimeDispatchError::InternalDispatchUnavailable`: DurableState `CommittedCorruptPoisoned`/`IndeterminatePoisoned`, or a post-commit required live-publication invariant poison such as owner-settled loaded Workspace installation failure, settles all joined in-process dispatch waiters with that `Err`. It does not claim mutation absence or rejection. Transport sends it if possible then closes, otherwise closes the connection; hosts query/reopen and must not blind-retry Create/Fork. `RuntimeClosed` is for later requests after close. If marker plus exact payload proves Published but a post-marker sync fails, DurableState first installs catalog and fulfills all waiters with Completed, then enters Closing/shutdown—failure-induced Closing cannot overtake completion. If host shutdown is already Closing, its durable-job settlement phase cannot pass until that Published completion has been fulfilled. No new wire outcome, variant, or code exists; see [ADR 0136](../adr/0136-durablestate-operation-owned-generations.md).

### Capability Matrix

| 能力 | 修改状态 | 启动异步工作 | 直接返回业务数据 | 产生StateEvent | 产生ProgressEvent |
| --- | --- | --- | --- | --- | --- |
| Command | 可以 | 可以 | 返回Completed outcome/CommandOutput或typed Rejected completion | 可以 | 间接可以 |
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
- `ToolCallId`：ModelGateway adapter归一化的provider/tool protocol correlation，不替代ItemId；协议无原生ID时生成response-local opaque ID，只要求同一assistant response内唯一；
- `EntryId`：Session history tree identity，由`LiveSessionState` private Session-scoped generator在apply前分配并由Recorder原样保存；默认不作为普通UI mutation input；Fork复制历史EntryId，因此其唯一性scope是Session；
- `CommandId`：public command correlation和in-flight dedup identity；Submit的CommandId同时标识领域Turn创建前的process-local admission candidate；

ID/revision的exact typed-prefix carrier、scope-preserving decode和Timestamp/u64规则由[Wire Schema](wire-schema.md#shared-scalar-carriers)统一拥有。public Rust newtype仍使cross-type value无法构造；wire prefix在JSON boundary提供第二层检查。

不公开：

- `RunId`；
- `WorkspaceId`；
- `execution_version`；
- `OperationType`；
- provider transport internals；
- Tool executor route；
- Workspace security signal。

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

Rust内部优先使用route enum，使非法坐标组合无法构造。Wire严格使用[ADR 0134 adjacent-tagged enum](wire-schema.md#enum-representation)，不得由adapter改投影为另一套flat route fields。

## Command

Command 表达 mutation、lifecycle operation、Turn control或用户命令解释请求。

### Command Envelope

```rust
pub struct CommandRequest {
    pub command_id: CommandId,
    pub command: RuntimeCommand,
}
```

`CommandId`由调用adapter在dispatch前生成，必须使用不可预测随机值且不得复用于另一条命令。当前Runtime内原命令仍in-flight时，相同`CommandId + exact typed command`重试加入同一completion；同一in-flight CommandId携带不同command返回`CommandCompletion::Rejected(CommandError { code: CommandConflict, ... })`。它只属于process-local/in-flight dedup：不持久化、不传给DurableState、不提供completed cache或durable-outcome query，restart后也不能重放/恢复。Create/Fork publication后若crash或response loss，generated ID可能catalog-visible但host未收到且无法唯一关联；host必须重新page/query catalog，blind retry可能创建duplicate。这是V1明确不提供restart exactly-once Create/Fork的限制。每个Command仍使用expected revision/status/TurnId表达乐观并发。

### RuntimeCommand

```rust
pub enum RuntimeCommand {
    Runtime(RuntimeLifecycleCommand),
    Agent(AgentCommand),
    Session(SessionCommand),
    Turn(TurnCommand),
    Interaction(InteractionCommand),
    CommandSurface(CommandSurfaceCommand),
}

pub enum RuntimeLifecycleCommand {
    ReloadSharedResources,
}
```

`ReloadSharedResources`对应`/reload`，发布新的共享Prompt/Skill/Tool/Model immutable objects；它不修改任何SessionDefinitionRevision，也不更新active/completed Turn。

`RuntimeCommand`只组合public protocol DTO和各domain owner公开的safe semantic leaf type。protocol crate从canonical owner re-export以下leaf，不在Runtime Interface复制第二套定义：

| Public leaf | Canonical owner |
| --- | --- |
| `AgentPromptSelection`、`SessionPromptSelection`、`PromptBodyIntent`、`TextIntent`、`SkillIntent` | [Prompt](prompt.md) |
| `WorkspaceDefinitionInput`、`WorkspaceRootInput`、`WorkspaceCwdSpec`、`RequestedFilesystemAccess`、`WorkspaceSourcePolicy` | [Workspace](workspace.md) |
| `SessionModelConfig` | [Agent/Session Lifecycle](agent-session-lifecycle.md)；其中Model leaf由[ModelGateway](model-gateway.md)拥有 |
| `ToolApprovalDecisionInput`、`UserQuestionAnswer`及question value types | [Tools](tools.md#approval-and-question-types) |
| `InteractionResolutionKey`、`InteractionCancelReason` | [Turn / Item / Interaction](turn-item-interaction.md#interaction) |

这些owner公开的是同一个Rust semantic type，不是Runtime-local shadow DTO。owner-assigned ID/revision/timestamp、private PermissionSet、source authorization和executor handle仍不能经这些leaf进入command。ProviderId/ModelId等base identity carrier与Workspace path的exact wire text由[Wire Schema](wire-schema.md#shared-scalar-carriers)统一拥有。

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

pub enum AgentUsableStatus {
    Enabled,
    Disabled,
}

pub struct NewAgentDefinition {
    pub prompts: AgentPromptSelection,
}

pub struct AgentDefinitionPatch {
    pub prompts: Option<AgentPromptSelection>,
}

pub struct NewAgentMetadata {
    pub name: String,
    pub description: Option<String>,
}

pub struct AgentMetadataPatch {
    pub name: Option<String>,
    pub description: OptionalTextPatch,
}

pub enum OptionalTextPatch {
    Keep,
    Set(String),
    Clear,
}
```

`AgentUsableStatus`只允许`Enabled | Disabled`。`AgentStatus::Deleted`只能通过`Delete`进入，Deleted identity不复用。`NewAgentDefinition`不接受AgentId/revision/timestamp；owner在Create时分配。patch至少包含一个非`None/Keep`字段，否则在CAS成功后归约为`NoChange`。name必须non-empty；description的Keep/Set/Clear避免用嵌套Option猜测“未修改”和“清空”。所有strings/collection受ProtocolLimits约束。

### SessionCommand

```rust
pub enum SessionCommand {
    Create {
        agent_id: AgentId,
        definition: Box<NewSessionDefinition>,
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
    ReloadWorkspace {
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

pub struct NewSessionDefinition {
    pub workspace: WorkspaceDefinitionInput,
    pub model: SessionModelConfig,
    pub prompts: SessionPromptSelection,
}

pub struct SessionDefinitionPatch {
    pub workspace: Option<WorkspaceDefinitionInput>,
    pub model: Option<SessionModelConfig>,
    pub prompts: Option<SessionPromptSelection>,
}

pub struct WorkspaceDefinitionInput {
    primary_root: WorkspaceRootInput,
    additional_roots: Vec<WorkspaceRootInput>,
    cwd: WorkspaceCwdSpec,
}

pub struct NewSessionMetadata {
    pub name: Option<String>,
    pub description: Option<String>,
}

pub struct SessionMetadataPatch {
    pub name: OptionalTextPatch,
    pub description: OptionalTextPatch,
}
```

`Box<NewSessionDefinition>`只控制command enum尺寸；它不建立新owner、不改变value semantics，也不改变public JSON V1 shape。

`WorkspaceDefinitionInput`没有WorkspaceRevision；它是Workspace-owned、host-neutral的command intent，root path保存Wire-owned typed `CanonicalFileUri` carrier，不保存native `PathBuf`。Wire完成URI lexical validation，Workspace constructor随后验证root count、duplicate key/URI、cwd引用、requested access和source policy等host-neutral invariant；这些失败表示typed command尚未形成，按input decode failure处理。typed command进入Runtime后，Workspace在Create/Update candidate validation中按current host family checked-lower为durable `WorkspaceRootSpec { path: PathBuf }`。unsupported host family或无法lossless形成native path返回`CommandError::InvalidArgument + DoNotRetry`，不是outer `RuntimeDispatchError`。native path、containment与authority invariant由Workspace lowering/resolution拥有。Session owner只在lowering成功后分配new Workspace revision。`SessionDefinitionPatch`为空时在CAS成功后为`NoChange`；Workspace/Model/Prompt字段各自是complete replacement candidate，不使用partial nested patch。metadata OptionalTextPatch同样区分Keep/Set/Clear。

`ReloadSharedResources`对应`/reload`：Runtime完整build Prompt/Skill/Tool/Model candidates，validate所有required candidates，然后在短publication gate下原子替换current immutable objects；任一required candidate失败时保留全部old current values并返回`ReloadValidationFailed`，不发布`RuntimeReloaded`或`shared_resources_reloaded`。active/completed Turn不更新，future Turn捕获reload后的objects。

`ReloadWorkspace`对应`/reload workspace`：只作用于loaded Session的current Workspace state，不修改SessionDefinitionRevision、metadata、conversation或Recorder。Session必须Idle且无active publication；Runtime经residency per-Session gate路由（loaded-only，不读取/更新DurableState），复用executor既有single active publication slot，worker重新resolve exact installed definition.workspace、由PromptService捕获Workspace-bound sources（Skill capture仍fail-closed为空）、required authority revalidation后finish exact WorkspaceSnapshot；成功原子替换Snapshot并发布Session-scope `session_workspace_reloaded`（detail null，Runtime scope不发事件），失败保留old Snapshot且不发布任何事件。Starting/Running/Finishing或已有active publication返回`SessionBusy`（不排队、不隐式Cancel、不原地替换active Turn Context）；unloaded Session返回`SessionNotLoaded`。resolver RootUnavailable/CanonicalizationFailed/AuthorityUnavailable或Prompt SourceDiscovery→`Unavailable`+RetryWithBackoff，AuthorityDenied→`Unauthorized`+UserActionRequired，RootNotDirectory/DuplicateRoot/OverlappingRoots/CwdOutsideRoots/CwdRootMismatch或Prompt ContentLoad/DuplicateKey→`ReloadValidationFailed`+UserActionRequired。Unavailable（WorkspaceUnavailable/PromptUnavailable）+ Idle的loaded Session同样接受reload：成功安装exact WorkspaceSnapshot并恢复Ready（发布同一`session_workspace_reloaded`），普通失败保持原Unavailable cause且不安装、不发事件。

`SessionDefinitionPatch`原子修改Workspace、SessionModelConfig或SessionPromptSelection，并生成新的`SessionDefinitionRevision`。修改Agent reference必须走`UpgradeAgentRevision`。若patch改变Workspace且Session已loaded，只在`SessionExecutionState::Idle`接受；Starting/Running/Finishing返回typed `SessionBusy`，不排队、不隐式Cancel。Host需要显式`Cancel → wait session_settled → UpdateDefinition`。

`Create` 只接受 `agent_id`：Runtime 在创建的 Agent lifecycle synchronization 内读取该 Agent 当时的 current revision，并把它作为 exact `AgentRevisionRef` 钉进 `SessionDefinition`。调用方不在 create 时报 revision——「用哪一版」由 Runtime 在此刻快照 current 决定，之后 Agent 再发布新 revision 不会改变该 Session（snapshot-current）。

`UpgradeAgentRevision.target` 为 `Option`：缺省（`None`）表示「重新钉到该 Agent 当前 current」（显式revision upgrade），是常规路径；给出 exact `AgentRevisionRef` 表示钉到指定版本（可用于钉旧版或回滚）。两种情况都在 gates 内校验 target 属于同一 AgentId、Agent 为 Enabled、target definition 存在，并原子解析为 exact ref 后写入新的 `SessionDefinitionRevision`；`latest` 本身不进入 durable `SessionDefinition`。保持exact pin让同一Session在两次upgrade之间的Agent selection、Workspace和Model配置稳定；显式Prompt resource reload仍只影响future Turn。

同一Session不提供原地history checkout。创建历史分支使用`Fork`，得到新的SessionId和独立definition revision序列。

### ForkAnchor

```rust
pub enum ForkSourceKind {
    LiveSnapshot,
    RecordedHistory,
}
```

`ForkSourceKind`由Runtime在source residency linearization point确定：source已loaded时为`LiveSnapshot`，未loaded时为`RecordedHistory`。调用方不能请求loaded source退回RecordedHistory，也不能根据recording health推断source。该public projection消费[INV-004](../architecture.md#跨模块不变量索引)，source selection与copy的完整规则由Conversation Recording canonical owner定义。

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

Wire uses the mixed-enum rule from Wire Schema: `ForkAnchor::Genesis` is exactly `{"type":"genesis"}` with `data` omitted; `data:null` and `data:{}` are invalid. Every payload anchor is exactly `{"type":"...","data":{"itemId":"..."}}`.

Runtime在已确定的Fork source内解析公开anchor：loaded source在同一个LiveSnapshot内解析anchor并 semantic-re-encodes selected path, unloaded source在RecordedHistory中解析。解析结果是target staging的合法path end：

- `BeforeUserMessage(Input/Steer)`解析到该message的parent；
- `AfterUserMessage`包含source中的对应UserMessage；该Item已live apply即可，不要求source record attempt已经成功；
- `Before/AfterFinalAgentMessage`只接受source中`phase = Final`的AgentMessage Item；
- anchor可以产生没有Final AgentMessage的conversation tail；fork原样复制该path，child不继承source current Turn；
- intermediate AgentMessage、Reasoning、ToolInvocation、Interaction和裸ToolResult不作为普通UI anchor；
- cross-session、stale或kind不匹配的ItemId返回typed error。

### TurnCommand

Runtime command边界使用与domain intent同构的非递归输入；容器在进入Session ingress前规范化为PromptIntent：

```rust
pub struct PromptIntentInput {
    pub body: PromptBodyIntent,
    pub skills: Vec<SkillIntent>,
}
```

`PromptIntentInput`没有独立Skill、Composite或Template variant。MVP body只允许`Empty | Text(TextIntent)`；Text必须在boundary normalization后non-empty并满足ProtocolLimits。SkillIntent只携带SkillId；slash name和GUI catalog selection必须先resolve为SkillId。Runtime边界执行shape/size与重复SkillId校验；exact Skill存在性、captured source读取与authorization由TurnExecutionContext在Submit admission或Steer safe point完成。任一composition失败都不apply部分UserMessage；具体PromptError到command/event的映射使用本module canonical table。serde tag/casing使用[Wire Schema JSON v1](wire-schema.md#json-v1-conventions)。

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
    },
}

pub enum PublicCancelTarget {
    Submit(CommandId),
    Turn(TurnId),
}
```

语义：

- `Submit`只在Session可以admit新Turn（Idle admission decision）时使用；Session非Idle时返回`SessionBusy`，不排队跨Turn等待；Turn Running期间的用户输入应由adapter路由为`Steer`（交互式默认）或`FollowUp`；initiating UserMessage成功apply到live state并完成inline record attempt后返回`TurnStarted`。该publication顺序消费[INV-001](../architecture.md#跨模块不变量索引)；
- `Steer`只作用于expected Running Turn；成功进入该Turn的bounded `SteerQueue<TurnId>`后返回`Queued`；
- `FollowUp`进入bounded process-local FIFO；返回`Queued`；
- `CancelQueuedMessage`只删除尚在Steer/FollowUp FIFO中的目标消息；未找到统一返回typed `QueuedMessageNotQueued`，不区分从未排队与已经出队，消息不会重新入队；
- `Cancel(Submit(command_id))`允许取消该CommandId对应、尚处于排队或Starting admission的Submit；Starting覆盖async context capture/Skill composition、Input live apply、record attempt与`TurnStarted` publication。Input apply前user Cancel使candidate resolve future失效、不创建Turn，并使原Submit完成`SubmitCancelled`；Input已live apply但response尚未发布时Cancel仍绑定同一Turn、阻止ActiveTurnTask spawn，原Submit完成`TurnStarted { turn_id }`，随后发布live interruption。调用方收到`TurnStarted`后使用`TurnId`取消；
- `Cancel(TurnId)`通过per-session target-scoped sticky`EmergencyControl`校验active Turn target并发布cancel epoch；成功后立即返回typed`CancelAccepted`，不等待普通work lane、Tool settlement或terminal publication；stale target不影响新Turn；
- Cancel current Turn清理该Turn尚未append的Steer，默认保留FollowUp，并在Finishing期间继续允许新的FollowUp进入bounded FIFO；停止全部queued work需要未来显式`StopAll`/`ClearQueuedMessages`能力；
- Turn terminal后迟到Steer或Cancel返回typed stale/terminal outcome。

Queued Steer和FollowUp仍是process-local值。Runtime restart后未被SessionRecorder写入的input不会恢复。

### InteractionCommand

```rust
pub enum InteractionCommand {
    Resolve {
        session_id: SessionId,
        expected_turn_id: TurnId,
        item_id: ItemId,
        request_id: RequestId,
        resolution: InteractionResolutionInput,
        resolution_key: InteractionResolutionKey,
    },
}

pub enum InteractionResolutionInput {
    ToolApproval(ToolApprovalDecisionInput),
    UserAnswer(UserQuestionAnswer),
    Cancelled,
}
```

Interaction request/resolution遵守：

```text
InteractionRequested live apply + inline record attempt
→ StateEvent::InteractionRequested
→ host提交Resolve
→ InteractionResolved live apply + inline record attempt
→ StateEvent::InteractionResolved
→ resume waiter或允许后续side effect
```

`InteractionResolutionInput`必须与request family匹配：ToolApproval只接受request提供的allow option index或Deny；UserQuestion接受完整`UserQuestionAnswer`；Cancelled关闭任一family。所有identity、index、required field、choice和family校验由MiniCore执行，不能信任UI本地状态。

`InteractionResolutionKey`由Presentation Adapter为一次logical resolution action生成，必须不可预测且在retry exact same payload时复用。scope为exact Session/Turn/Item/Request：same key + same canonical input幂等；same key + different input返回CommandConflict；different key after terminal返回InteractionAlreadyResolved。key不是approval capability或authorization secret。

MiniCore拥有Interaction protocol：它创建`RequestId`并绑定Turn/Item，在live state中管理request/resolution并best-effort record，处理Cancel、terminal cleanup、Unload和幂等。用户长时间无回答、暂时没有subscriber或transport断开都保持Pending，不产生默认Deny。Presentation Adapter只把UI-safe request渲染并提交resolution；它不能自行创建MiniCore未请求的Pending Interaction，也不能直接持有Tool waiter或SessionRecorder。

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

pub struct CommandSelection {
    pub path: Vec<String>,
}

pub struct CommandArgs {
    pub values: Vec<CommandArgInput>,
}

pub enum CommandArgInput {
    Positional(CommandArgumentValueInput),
    Named {
        name: String,
        value: CommandArgumentValueInput,
    },
}

pub enum CommandArgumentValueInput {
    Text(String),
    Boolean(bool),
    Choice(String),
}

pub enum CommandPromptDelivery {
    Submit,
    Steer {
        expected_turn_id: TurnId,
    },
    FollowUp,
}
```

`CommandSelection.path`必须与current safe catalog中的exact command path匹配；它不是internal handler ID。`CommandArgs`保留ordered positional/named typed values，`Choice(String)`必须匹配catalog allowlist；不能嵌入RuntimeCommand、arbitrary JSON或credential。Runtime在execution时重新materialize catalog、re-resolve path并按command-specific schema验证name/order/count/type/value；所有文本和数量受ProtocolLimits约束。

`delivery`只对产生PromptIntent的command生效。`/status`、`/help`、`/model`等command忽略该字段。

`delivery = None`时默认解析为`Submit`。交互式UI应按观察到的Session状态路由：Turn Running时默认`Steer { expected_turn_id }`，用户显式选择"排到下一轮"时使用`FollowUp`。Steer因Turn terminal返回typed stale/terminal outcome时，adapter把同一输入改发为`Submit`（开启新Turn）或提示用户；Submit因竞态返回`SessionBusy`时，adapter按新Snapshot重新路由。运行中输入的解释属于adapter层，Runtime不把Submit静默转换为Steer。

### Command Response

```rust
pub struct CommandResponse {
    pub command_id: CommandId,
    pub completion: CommandCompletion,
}

pub enum CommandCompletion {
    Completed {
        outcome: CommandOutcome,
        output: Option<CommandOutput>,
    },
    Rejected(CommandError),
}

pub struct CommandOutput {
    pub text: String,
}
```

MVP `CommandOutput`只承载bounded、redacted plain text；不承载HTML、ANSI escape、Markdown action、embedded RuntimeCommand或credential。rich catalog/help数据使用typed QueryResult。exact 65,536-byte limit与safe-text wire由[Wire Schema ProtocolLimits](wire-schema.md#protocollimits-v10)拥有。

```rust
pub enum CommandOutcome {
    AgentCreated {
        agent_id: AgentId,
        definition_revision: AgentRevision,
        metadata_revision: AgentMetadataRevision,
    },
    AgentDefinitionUpdated {
        definition_revision: AgentRevision,
    },
    AgentMetadataUpdated {
        metadata_revision: AgentMetadataRevision,
    },
    AgentStatusChanged {
        status: AgentStatus,
    },
    AgentDeleted,
    SessionCreated {
        session_id: SessionId,
        definition_revision: SessionDefinitionRevision,
        metadata_revision: SessionMetadataRevision,
    },
    SessionDefinitionUpdated {
        definition_revision: SessionDefinitionRevision,
    },
    SessionMetadataUpdated {
        metadata_revision: SessionMetadataRevision,
    },
    SessionLoaded,
    SessionUnloaded,
    SessionArchived,
    SessionUnarchived,
    SessionDeleted,
    SharedResourcesReloaded,
    WorkspaceReloaded,
    SessionForked {
        session_id: SessionId,
        source: ForkSourceKind,
    },
    TurnStarted {
        turn_id: TurnId,
    },
    SubmitCancelled,
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

Command completion不是完整业务完成流：

- command一旦通过envelope validation进入Runtime，领域/admission成功或拒绝都通过同一个`CommandResponse.command_id`完成；`RuntimeDispatchError`不承载普通Stale/Busy/NotFound。Only an integrity-fatal post-admission failure settles joined waiters as existing `InternalDispatchUnavailable`: DurableState poison or post-commit required live-publication invariant poison, with no semantic completion and no new public wire outcome. Published plus valid readback followed by a sync failure instead completes all waiters before Closing；
- `Completed.output`只有`outcome = CommandOutput`时为Some，其他outcome为None；`Rejected`没有CommandOutput；
- Agent/Session create返回definition与metadata两个独立revision；definition、metadata、status/lifecycle mutation使用不同outcome，caller不能把metadata token误当execution revision；
- metadata canonical no-op返回`NoChange`并保持原token，不发布metadata event；
- `SessionForked.source`精确报告该Fork使用`LiveSnapshot`还是`RecordedHistory`；同一值保存在child durable provenance；
- `SubmitCancelled`只表示用户Cancel在initiating UserMessage live apply前获胜；没有TurnId、UserMessage、TurnStarted或Turn terminal event。Input live apply先赢时原Submit仍返回`TurnStarted { turn_id }`，随后同一Turn按Cancel路径发布`TurnInterrupted`；SecurityRevoked、Unload或Runtime shutdown使用各自typed rejection，不伪装成SubmitCancelled；graceful Unload的deadline在Input live apply前先赢时Starting Submit公开映射`SessionNotLoaded`（internal `PrepareForUnload`重分类），Input live apply先赢时仍`TurnStarted`并随后发布`TurnInterrupted(PrepareForUnload)`；
- `TurnStarted`只表示initiating UserMessage已live apply并完成当前record attempt，领域Turn已在当前loaded Session创建；它不是durable Turn-start receipt；
- Turn最终`Completed | Interrupted | Failed`由StateEvent发布；
- `CancelAccepted`只表示target仍可取消且sticky cancel epoch已经发布。Input live apply前它使正在进行的capture/composition future失效并取消candidate；Input已apply但`TurnStarted`尚未发布时，它绑定同一Turn并阻止task spawn；active Turn中则与live Completed decision first-wins，accepted后Completed不得再赢。Tool清理和Turn terminal通过后续StateEvent/Snapshot观察；
- `SteerQueued`和`FollowUpQueued`只表示当前Runtime的对应SessionIngress lane已接收，不承诺crash-safe delivery；matching CommandId立即出现在new SessionSnapshot的lane-local queue Vec中。真正append通过普通UserMessage/Turn StateEvent观察；restart后未append的消息和queue Vec都消失；
- `SessionLoaded`在load/recovery完成并原子发布latest SessionSnapshot后返回；recording health从随后显式`Snapshot(Session)`或subscription首帧读取，不内联进unit outcome；
- slash command的display-neutral结果可以直接放入`CommandOutput`；
- Session/Agent事实变化同时发布StateEvent，让其他subscriber失效或更新read model。

### Command 线性化点

| Command | Response线性化点 |
| --- | --- |
| Create Agent | DurableState establishes complete publication and installs immutable catalog; response loss may leave the published ID unknown. |
| Update Agent Definition | expected AgentRevision/status CAS成功，返回new definition revision或NoChange |
| Update Agent Metadata | expected AgentMetadataRevision/status CAS成功，返回new metadata revision或NoChange |
| Create Session | DurableState establishes complete publication-time Agent pin and immutable Open + Unloaded catalog; response loss may leave the published ID unknown. |
| Update SessionDefinition（non-Workspace或unloaded Workspace） | new definition revision durable publication |
| Update Session Metadata | expected SessionMetadataRevision/lifecycle CAS成功，返回new metadata revision或NoChange |
| Update loaded Session Workspace | Idle校验、new revision durable publication和new WorkspaceSnapshot Ready publication全部完成 |
| Upgrade Agent Revision | exact same-Agent pinned new definition revision durable publication（target current解析与Enabled/retained校验只在DurableState `Agent → Session` gates内完成）；unloaded直接发布definition，loaded经executor既有publication slot原子安装exact definition并发布Runtime+Session `SessionDefinitionUpdated`事件；NoChange不发布 |
| Reload shared Prompt/Model | Prompt/Model candidates全部validate并在shared-resource write gate下fan-out new roots至全部loaded executors后一次原子替换Runtime root pair；失败时old roots/executors保持不变且无事件 |
| Load Session | single-flight load/recovery完成并发布readiness |
| Reload Workspace | Session Idle校验、Workspace resolve和Workspace-bound Prompt/Skill source capture全部完成后替换Snapshot；非Idle返回SessionBusy |
| Unload Session | LifecycleControl完成grace/fail-closed task settlement并从loaded map移除；Recorder无后台drain |
| Submit | async captured contribution resolve完成并通过candidate/control/emergency/authority重验后，initiating UserMessage live apply与inline record attempt；若user Cancel在apply前先赢，原Submit完成为SubmitCancelled |
| Steer Queued | target Turn的SteerQueue admission |
| FollowUp | FollowUpQueue admission |
| CancelQueuedMessage | target仍在任一FIFO时remove；否则返回QueuedMessageNotQueued |
| Resolve Interaction | InteractionResolved live apply与inline record attempt |
| Cancel Submit(CommandId) | active target仍可取消且sticky cancel epoch发布；user Cancel在Input apply前先赢时原Submit完成`SubmitCancelled`，Input apply先赢时原Submit完成`TurnStarted`并随后TurnInterrupted；target已转换为published Turn时返回SubmitNotCancellable |
| Cancel Turn | active target仍可取消且sticky cancel epoch发布；live final arbitration先完成时返回typed terminal/transition error；最终TurnInterrupted由StateEvent/Snapshot表达 |
| Fork Session | source kind/semantic seed is captured, actor-owned semantic re-encode fully materializes/validates the child, and DurableState establishes complete publication and immutable catalog; response loss may leave the child ID unknown. |

### Command Error

```rust
pub struct CommandError {
    pub code: CommandErrorCode,
    pub message: String,
    pub retry: RetryAdvice,
    pub subject: Option<PublicSubject>,
}

pub enum CommandErrorCode {
    InvalidArgument,
    NotFound,
    CommandConflict,
    StaleRevision,
    AgentDisabled,
    AgentDeleted,
    SessionArchived,
    SessionDeleted,
    SessionNotLoaded,
    SessionNotReady,
    SessionBusy,
    ReloadValidationFailed,
    IngressLaneFull { lane: PublicIngressLane },
    QueuedMessageNotQueued,
    SubmitNotCancellable,
    ExpectedTurnMismatch,
    TurnNotRunning,
    TurnCancelling,
    TurnTerminal,
    InteractionNotFound,
    InteractionAlreadyResolved,
    InteractionFamilyMismatch,
    InvalidForkAnchor,
    Unauthorized,
    Unavailable,
    DurableStateCorrupt,
    DurableStateTooLarge,
    RuntimeClosing,
}

pub enum PublicIngressLane {
    TurnAdmission,
    Steer,
    FollowUp,
    InteractionControl,
    ToolControl,
}

pub enum RetryAdvice {
    DoNotRetry,
    RefreshAndRetry,
    RetryWithBackoff {
        retry_after: Option<Duration>,
    },
    UserActionRequired,
}

pub enum PublicSubject {
    Runtime,
    Command(CommandId),
    Agent(AgentId),
    Session(SessionId),
    Turn { session_id: SessionId, turn_id: TurnId },
    Item { session_id: SessionId, turn_id: TurnId, item_id: ItemId },
    Interaction {
        session_id: SessionId,
        turn_id: TurnId,
        item_id: ItemId,
        request_id: RequestId,
    },
    Skill(SkillId),
}
```

`message`是bounded、redacted、仅供人读的补充；外部调用方不能解析它决定retry。`code + retry + subject`是唯一machine contract。`IngressLaneFull`只允许上面的五个public work lane；EmergencyControl、LifecycleControl和SnapshotMailbox不产生该错误。

`RuntimeDispatchError`通常用于无法形成command completion的envelope/runtime入口故障；唯一例外是既有`InternalDispatchUnavailable`也承载post-admission integrity-fatal settlement：

```rust
pub enum RuntimeDispatchError {
    InvalidEnvelope,
    RequestTooLarge,
    RuntimeClosed,
    InternalDispatchUnavailable,
}

pub struct QueryError {
    pub code: QueryErrorCode,
    pub message: String,
    pub retry: RetryAdvice,
    pub subject: Option<PublicSubject>,
}

pub enum QueryErrorCode {
    InvalidArgument,
    NotFound,
    SessionNotLoaded,
    StaleCursor,
    ResultTooLarge,
    Unavailable,
    DurableStateCorrupt,
    DurableStateTooLarge,
    RuntimeClosing,
}

pub struct SnapshotError {
    pub code: SnapshotErrorCode,
    pub message: String,
    pub retry: RetryAdvice,
    pub subject: Option<PublicSubject>,
}

pub enum SnapshotErrorCode {
    NotFound,
    SessionNotLoaded,
    Unavailable,
    RuntimeClosing,
}

pub struct SubscriptionError {
    pub code: SubscriptionErrorCode,
    pub message: String,
    pub retry: RetryAdvice,
    pub subject: Option<PublicSubject>,
}

pub enum SubscriptionErrorCode {
    UnsupportedScope,
    NotFound,
    SessionNotLoaded,
    PublisherUnavailable,
    RuntimeClosing,
}
```

同一in-flight CommandId携带不同command已经进入dedup owner，因此返回`CommandCompletion::Rejected(CommandError { code: CommandConflict, ... })`；不是transport error。

Query/Snapshot/Subscription error同样只携带closed code、bounded redacted message、RetryAdvice和optional subject。`StaleCursor`表示cursor与query family/filter/sort/captured immutable snapshot不匹配，或其snapshot已因restart、bounded eviction或scope unload不可用；调用方必须从first page重启。`ResultTooLarge`用于首版明确不分页的完整read model超过ProtocolLimits，不能静默截断破坏自洽性。Snapshot/Subscription的SessionNotLoaded不会隐式Load。publisher关闭或subscriber背压造成当前stream终止后，host重新subscribe获取new Snapshot；不能用旧SubscriptionError恢复event cursor。

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
- 直接写Conversation Storage；
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
→ resolve code-review → SkillId
→ PromptIntentInput {
     body: Text("修复这个问题"),
     skills: [SkillIntent { skill_id }]
   }
→ delivery选择Submit、Steer或FollowUp
→ 对应TurnCommand
```

用户显式Skill选择最终成为UserMessage/Steer的一组content parts，不创建独立Item。模型发起Skill Tool时仍使用ToolInvocation Item与ToolCall/ToolResult协议。

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

本节必须区分**current v0.1 exact surface**与**post-MVP target sketch**。只有下面第一段列出的variants已经存在于`RuntimeQuery/QueryResult`与Wire V1；后面的CommandSurface/Model/Prompt/Skill/Tool/Usage/Diagnostics及完整history query只是未来设计方向，不能由前端当作当前可调用接口。

```rust
pub enum RuntimeQuery {
    Runtime(RuntimeReadQuery),
    Agent(AgentQuery),
    Session(SessionQuery),
}

pub struct PageRequest {
    cursor: Option<PageCursor>,
    limit: NonZeroU32,
}

impl PageRequest {
    pub const fn new(cursor: Option<PageCursor>, limit: NonZeroU32) -> Self;
    pub const fn cursor(self) -> Option<PageCursor>;
    pub const fn limit(self) -> NonZeroU32;
}
```

### RuntimeReadQuery

```rust
pub enum RuntimeReadQuery {
    GetCapabilities,
}
```

### AgentQuery

```rust
pub enum AgentQuery {
    ListAgents {
        page: PageRequest,
        include_deleted: bool,
    },
}
```

### SessionQuery

```rust
pub enum SessionQuery {
    ListSessions {
        page: PageRequest,
        include_archived: bool,
    },
    GetSessionForkProvenance {
        session_id: SessionId,
    },
}
```

### v0.1 Session Transcript（Library-only）

completed聊天恢复不经过`RuntimeQuery`，而使用[ADR 0148](../adr/0148-v0-1-session-transcript-is-a-library-only-read-seam.md)冻结的Rust embedding method：

```rust
impl MiniCoreRuntime {
    pub async fn session_transcript(
        &self,
        session_id: SessionId,
        page: PageRequest,
    ) -> Result<Page<SessionTranscriptItem>, QueryError>;
}

pub enum SessionTranscriptItemRole {
    User,
    Assistant,
}

pub struct SessionTranscriptItem {
    // private fields; public getters expose ItemId, TurnId, role,
    // optional UserMessageSource, body and Timestamp.
}
```

exact合同：

- first page只接受loaded Session；未loaded返回`SessionNotLoaded + UserActionRequired + Session subject`；
- Session actor在串行点从canonical selected path捕获immutable entry `Arc`，不做storage I/O且guard不跨await；
- current Turn全部entries从capture排除，继续由`SessionSnapshot.active_items`与events展示；
- 只投影User正文与Assistant Text items；Reasoning、Tool、Interaction、Compaction不进入v0.1 transcript DTO；
- User multi-part以单个`\n`连接；顺序为selected path顺序与Assistant content顺序；
- page cursor绑定same Session与首次immutable capture，continuation不重新访问residency，Session后续append或Unload不改变已开始的分页；
- cursor与catalog query共用1..=200 page limit、15分钟TTL、4,096 capacity及one-shot successor；
- restart后先`Load`，transcript从tolerant replay实际恢复的recorded prefix读取；unrecorded live tail仍不可恢复；
- 该method不属于Wire V1，remote transport若需要它必须走独立protocol minor。

### Post-MVP History Query Targets

以下是完整history/navigation生态的target sketch，**当前未实现、未进入Wire V1，也不阻塞v0.1**：

```rust
pub enum FutureSessionHistoryQuery {
    GetHistoryTree {
        session_id: SessionId,
    },
    ListTurns {
        session_id: SessionId,
        page: PageRequest,
    },
    GetTurn {
        session_id: SessionId,
        turn_id: TurnId,
    },
    ListItems {
        session_id: SessionId,
        turn_id: TurnId,
    },
    GetItem {
        session_id: SessionId,
        turn_id: TurnId,
        item_id: ItemId,
    },
}
```

`GetHistoryTree`返回公开read model：

```rust
pub struct SessionHistoryTreeView {
    pub session_id: SessionId,
    pub current_anchor: HistoryAnchorView,
    pub nodes: Vec<HistoryNodeView>,
}

pub enum HistoryAnchorView {
    Genesis,
    UserMessage { item_id: ItemId },
    FinalAgentMessage { item_id: ItemId },
}

pub struct HistoryNodeView {
    pub anchor: HistoryAnchorView,
    pub parent: Option<HistoryAnchorView>,
    pub turn_id: Option<TurnId>,
    pub timestamp: Option<Timestamp>,
    pub orphan_root: bool,
    pub diagnostic_count: u32,
}
```

```rust
pub struct SessionForkProvenanceView {
    pub source_session_id: SessionId,
    pub source: ForkSourceKind,
    pub anchor: ForkAnchor,
}
```

non-fork Session的`GetSessionForkProvenance`返回`None`；fork child返回durable provenance。该query不重新判断source当前是否loaded，也不根据后续recording health改写`source`。

History node未来使用Turn/Item/message语义，不向普通UI暴露raw StoredSessionEntry、recorder internals或physical recorded head。`GetHistoryTree`若实现，将从owner一次性捕获完整compact branch topology，不内联Turn Item bodies，也不分页；它服务fork/navigation与损坏历史检查，不代替当前v0.1基础聊天timeline。

未来`ListTurns`与`GetTurn`中的历史Turn只会是按recorded `TurnId`分组的conversation segment；不会虚构`Running | Completed | Interrupted | Failed` execution status。current loaded Turn状态仍只从`SessionSnapshot.current_turn`和实时StateEvent读取。

未来history query也不返回或重放Session definition/metadata/lifecycle transition timeline；conversation JSONL只提供conversation facts。

v0.1当前分页面只有`ListAgents`、`ListSessions`与library-only `session_transcript`。长期Session的newest-first滚动、reverse pagination、`ListTurns`与catalog families均是post-MVP。所有现有cursor都绑定query family/filter或Session identity与captured immutable snapshot，调用方不能跨family/filter/Session复用或拼接page。

### Post-MVP Query Families

从这里开始的`CommandSurfaceQuery`、`ModelQuery`、`PromptQuery`、`SkillQuery`、`ToolQuery`、`UsageQuery`与`DiagnosticsQuery`同样是future target sketches；当前v0.1没有对应`RuntimeQuery` variant。

#### CommandSurfaceQuery

```rust
pub enum CommandSurfaceQuery {
    GetCatalog {
        session_id: Option<SessionId>,
    },
    Suggest {
        session_id: Option<SessionId>,
        input: String,
        cursor: CommandCursorPosition,
    },
    GetHelp {
        session_id: Option<SessionId>,
        command_path: Vec<String>,
    },
}

pub struct CommandCursorPosition {
    pub utf8_byte_offset: u32,
}
```

`CommandCursorPosition`必须落在input的UTF-8 code-point boundary且不大于input byte length；否则QueryError::InvalidArgument。这样in-process Rust和future wire adapter共享一个无歧义位置语义。

### ModelQuery

```rust
pub enum ModelQuery {
    ListProviders {
        page: PageRequest,
    },
    ListModels {
        provider_id: Option<ProviderId>,
        page: PageRequest,
    },
    GetModelCapabilities {
        selection: ModelSelection,
    },
}
```

只返回safe model identity、display metadata、availability、context limit和capabilities。Endpoint、auth reference、credential和provider client不公开。

### Prompt / Skill / Tool Query

只提供UI-safe catalog和diagnostics：

```rust
pub enum PromptQuery {
    ListPrompts {
        page: PageRequest,
    },
    GetPromptSummary {
        prompt_id: PromptId,
    },
}

pub enum SkillQuery {
    ListSkills {
        session_id: Option<SessionId>,
        page: PageRequest,
    },
    GetSkillSummary {
        session_id: Option<SessionId>,
        skill_id: SkillId,
    },
}

pub enum ToolQuery {
    ListTools {
        session_id: Option<SessionId>,
        page: PageRequest,
    },
    GetToolSummary {
        session_id: Option<SessionId>,
        tool_name: ToolName,
    },
}

pub enum UsageQuery {
    GetSessionUsage {
        session_id: SessionId,
    },
}

pub enum DiagnosticsQuery {
    GetRuntimeDiagnostics,
    GetSessionDiagnostics {
        session_id: SessionId,
    },
    GetLoadedSessionDiagnostics,
}
```

Prompt catalog是shared current root；Skill/Tool query的`session_id = Some`使用loaded Session current safe view，`None`使用shared/global safe catalog并省略Workspace-bound或Session-filtered entries。它们不隐式Load Session。Diagnostics只返回bounded current safe projections；`GetLoadedSessionDiagnostics`不扫描unloaded durable catalogs。

Prompt/Skill正文和Tool private schema/route默认不进入普通Query。后续privileged debug interface必须独立授权。

### Query Response

```rust
pub struct QueryResponse {
    pub data: QueryResult,
}

pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<PageCursor>,
}

pub struct PageCursor(String);

pub enum QueryResult {
    Runtime(RuntimeQueryResult),
    Agent(AgentQueryResult),
    Session(SessionQueryResult),
    CommandSurface(CommandSurfaceQueryResult),
    Model(ModelQueryResult),
    Prompt(PromptQueryResult),
    Skill(SkillQueryResult),
    Tool(ToolQueryResult),
    Usage(SessionUsageView),
    Diagnostics(DiagnosticsQueryResult),
}

pub enum RuntimeQueryResult {
    Capabilities(RuntimeCapabilities),
    Info(RuntimeInfo),
    LoadedSessions(Vec<LoadedSessionSummary>),
}

pub enum AgentQueryResult {
    Agents(Page<AgentSummary>),
    Agent(AgentSummary),
    Revisions(Page<AgentDefinitionSummary>),
    Definition(AgentDefinitionSummary),
}

pub enum SessionQueryResult {
    Sessions(Page<SessionSummary>),
    Session(SessionSummary),
    Definition(SessionDefinitionSummary),
    Readiness(Option<SessionReadinessView>),
    ForkProvenance(Option<SessionForkProvenanceView>),
    HistoryTree(SessionHistoryTreeView),
    Turns(Page<HistoricalTurnSummaryView>),
    Turn(HistoricalTurnView),
    Items(Vec<ItemView>),
    Item(ItemView),
    PendingInteractions(Vec<InteractionView>),
}

pub enum CommandSurfaceQueryResult {
    Catalog(CommandCatalogView),
    Suggestions(Vec<CommandSuggestionView>),
    Help(CommandOutput),
}

pub enum ModelQueryResult {
    Providers(Page<ProviderSummaryView>),
    Models(Page<ModelSummaryView>),
    Capabilities(ModelCapabilitiesSummaryView),
}

pub enum PromptQueryResult {
    Prompts(Page<PromptSummaryView>),
    Prompt(PromptSummaryView),
}

pub enum SkillQueryResult {
    Skills(Page<SkillSummaryView>),
    Skill(SkillSummaryView),
}

pub enum ToolQueryResult {
    Tools(Page<ToolSummaryView>),
    Tool(ToolSummaryView),
}

pub enum DiagnosticsQueryResult {
    Runtime(Vec<RuntimeDiagnosticView>),
    Session(SessionDiagnosticsView),
    Loaded {
        runtime: Vec<RuntimeDiagnosticView>,
        sessions: Vec<SessionDiagnosticsView>,
    },
}

pub struct SessionDiagnosticsView {
    pub session_id: SessionId,
    pub diagnostics: Vec<SessionDiagnosticView>,
}
```

Query不发布Event、不创建Turn、不加载完整SessionExecutor来读取持久化目录，也不消费Submit/Steer/FollowUp queue。Query的多个独立调用不承诺形成跨调用原子Snapshot。每个request variant只能返回同family中matching result variant；mismatch是Runtime invariant failure，不降级为空值。domain CAS revision直接存在于`AgentSummary`、`SessionSummary`或definition result中，不再增加未定义generic `QueryRevision`。`PageCursor`使用[Wire Schema exact `pc1_` carrier与4096-entry/15-minute bounded store](wire-schema.md#interaction-key与cursor)，绑定exact family/filter/sort/captured immutable snapshot；restart、expiry、eviction或scope unload返回StaleCursor。`PageRequest.limit`超过ProtocolLimits时返回InvalidArgument。

### Common Read Views

```rust
pub struct RuntimeInfo {
    pub protocol_version: ProtocolVersion,
    pub implementation: String,
    pub implementation_version: String,
}

pub struct RuntimeCapabilities {
    pub values: Vec<RuntimeCapability>,
}

pub enum RuntimeCapability {
    StateEvents,
    ProgressEvents,
    RuntimeSnapshot,
    SessionSnapshot,
    PagedQueries,
    CommandCatalog,
    InteractionResolution,
    SessionFork,
}

pub struct AgentDefinitionSummary {
    pub agent_id: AgentId,
    pub revision: AgentRevision,
    pub prompt_ids: Vec<PromptId>,
    pub created_at: Timestamp,
}

pub struct SessionDefinitionSummary {
    pub session_id: SessionId,
    pub revision: SessionDefinitionRevision,
    pub agent: AgentRevisionRef,
    pub workspace: WorkspaceDefinitionSummaryView,
    pub model: SessionModelConfig,
    pub prompts: SessionPromptSelection,
    pub created_at: Timestamp,
}

pub struct WorkspaceDefinitionSummaryView {
    pub roots: Vec<WorkspaceRootSummaryView>,
    pub cwd: WorkspaceCwdSpec,
}

pub struct WorkspaceRootSummaryView {
    pub key: WorkspaceRootKey,
    pub requested_access: RequestedFilesystemAccess,
    pub sources: WorkspaceSourcePolicy,
}

pub struct HistoricalTurnSummaryView {
    pub turn_id: TurnId,
    pub has_final_agent_message: bool,
    pub first_timestamp: Timestamp,
    pub last_timestamp: Timestamp,
}

pub struct HistoricalTurnView {
    pub turn_id: TurnId,
    pub items: Vec<ItemView>,
    pub has_final_agent_message: bool,
    pub first_timestamp: Timestamp,
    pub last_timestamp: Timestamp,
}

pub struct RuntimeDiagnosticView {
    pub code: String,
    pub message: String,
}

pub struct SessionDiagnosticView {
    pub code: String,
    pub message: String,
}
```

`SessionDefinitionSummary`复用lifecycle-owned `SessionModelConfig`与Prompt-owned `SessionPromptSelection`，Wire分别投影为`model`与`promptIds`；不得再声明shadow summary DTO。`WorkspaceDefinitionSummaryView.roots[0]`固定为primary root，其余按definition order为additional roots；validated constructor显式分开`primary_root`与`additional_roots`后再形成wire Vec。

diagnostic view没有global severity。code由owning module allowlist投影，message bounded/redacted；raw provider/storage/OS text不公开。Runtime Interface不暴露可接受任意code/message的public constructor；只有owner-controlled safe projection与bounded receiver-side decode可以形成这些值。

### Diagnostic Projection Limits

owner先维护ordered diagnostic facts与per-code totals，public Vec再按target surface投影；禁止直接把Conversation Replay的`100 Detail + Truncated`内部records复制进Snapshot：

```text
SessionSnapshot.diagnostics             limit = 50
GetSessionDiagnostics / each scope      limit = 100
Runtime diagnostics query/snapshot      matching advertised scope limit
```

通用`project_diagnostics(limit)`规则：

1. synthetic internal `Truncated`不算source fact；source fact total和per-code totals来自owner typed counters；
2. 若全部source facts都有detail且`total <= limit`，按owner order返回全部detail，不创建summary；
3. 否则保留owner order前`limit - 1`条available detail，并以最后一个slot返回`code = diagnostics_truncated` summary；
4. public summary的`omitted_detail_count = total_source_fact_count - returned_detail_count`，per-code totals覆盖全部source facts并按code bytes升序；summary自身不回增total；
5. summary message固定bounded format `omitted <N> diagnostic details; totals: <code>=<count>,...`，仅供人读，consumer不得解析message；machine totals保留在owner/query harness，不新增generic public aggregate DTO。

Session Snapshot有一个额外current-state rule：当`recording.state = degraded`时，matching latest recording diagnostic固定为`diagnostics[0]`且只占一个slot；随后对排除该fact的其余ordered facts调用`project_diagnostics(49)`。若Healthy则对全部facts调用`project_diagnostics(50)`。这样Snapshot始终满足recording warning invariant且总count<=50。Session diagnostics Query不做该重排，按owner order使用limit 100。

101条同code replay facts因此得到：internal 100 Detail + typed Truncated；Session Query返回99 detail + summary（omitted=2、total=101）；Healthy Snapshot返回49 detail + summary（omitted=52、total=101）。

### Catalog Read Views

```rust
pub struct CommandCatalogView {
    pub entries: Vec<CommandCatalogEntryView>,
}

pub struct CommandCatalogEntryView {
    pub path: Vec<String>,
    pub title: String,
    pub description: Option<String>,
    pub accepts_prompt: bool,
    pub arguments: Vec<CommandArgumentView>,
}

pub struct CommandArgumentView {
    pub name: String,
    pub required: bool,
    pub position: Option<u32>,
    pub value: CommandArgumentValueView,
}

pub enum CommandArgumentValueView {
    Text,
    Boolean,
    Choice {
        values: Vec<String>,
    },
}

pub struct CommandSuggestionView {
    pub replace: CommandTextRange,
    pub replacement: String,
    pub display: String,
}

pub struct CommandTextRange {
    pub start_utf8_byte_offset: u32,
    pub end_utf8_byte_offset: u32,
}

pub struct ProviderSummaryView {
    pub provider_id: ProviderId,
    pub display_name: String,
    pub available: bool,
}

pub struct ModelSummaryView {
    pub selection: ModelSelection,
    pub display_name: String,
    pub context_window_tokens: Option<NonZeroU32>,
    pub max_output_tokens: Option<NonZeroU32>,
    pub capabilities: ModelCapabilitiesSummaryView,
    pub available: bool,
}

pub struct ModelCapabilitiesSummaryView {
    pub tools: bool,
    pub structured_output: bool,
    pub reasoning: bool,
    pub streaming: bool,
    pub parallel_tool_calls: bool,
}

pub struct PromptSummaryView {
    pub prompt_id: PromptId,
    pub display_name: String,
    pub role: PromptRole,
    pub available: bool,
}

pub struct SkillSummaryView {
    pub skill_id: SkillId,
    pub name: String,
    pub description: Option<String>,
    pub available: bool,
}

pub struct ToolSummaryView {
    pub name: ToolName,
    pub description: String,
    pub read_only: bool,
    pub destructive: bool,
    pub open_world: bool,
}
```

catalog views不包含Prompt/Skill正文、Tool schema/private route、provider endpoint/auth或Workspace absolute path。每个owner负责构造safe summary；ADR 0134只冻结wire/limits，不重新定义业务字段。

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
    Session(Box<SessionSnapshot>),
}
```

`Box<SessionSnapshot>`只控制enum尺寸，不改变semantic ownership或Wire V1 shape。当前 incremental codec materialize `Open + Loaded + Ready` 的 Idle baseline，以及 M9.18 已激活的 Starting/Running/Finishing queue projection；M9.20 已补齐 running/approval 快照中的 current Turn、active Items 与 Pending Interaction 最小安全投影（不暴露原始工具参数、工具结果或隐含推理）；Session usage、Degraded recording与bounded diagnostic projection已由Session Executor真实状态驱动，并在Wire V1中作为accepted/canonical snapshot状态处理。

### RuntimeSnapshot

```rust
pub struct RuntimeSnapshot {
    pub runtime: RuntimeView,
    pub loaded_sessions: Vec<LoadedSessionSummary>,
    pub diagnostics: Vec<RuntimeDiagnosticView>,
}

pub struct RuntimeView {
    pub status: RuntimeStatusView,
}

pub enum RuntimeStatusView {
    Running,
    Closing,
}

pub struct LoadedSessionSummary {
    pub session_id: SessionId,
    pub readiness: SessionReadinessView,
    pub execution: SessionExecutionView,
    pub recording: SessionRecordingView,
}
```

`RuntimeSnapshot`只恢复current-process Runtime状态与loaded membership，不无界内联durable Agent/Session catalog。host每次收到新的Runtime Snapshot（首次subscribe、reconnect或publisher restart）都必须丢弃本地Agent/Session catalog cache并重新执行paged `ListAgents`与`ListSessions`；随后当前subscription内的typed Runtime StateEvent用于增量失效/刷新。该两步规则使断线期间丢失的create/archive/delete可以恢复，又不建立all-catalog stop-the-world Snapshot。

`RuntimeSnapshot`不包含所有loaded Session的完整message、current Items、Pending Interaction或大型Prompt/Skill/Tool/Model/Command catalogs。它只负责Runtime scope状态和loaded membership；host收到每个新的Runtime Snapshot后按需重新执行safe catalog queries，不使用catalog revision判断本地cache是否仍有效。

`LoadedSessionSummary` legal matrix固定为：`Ready + Idle|Starting|Running|Finishing`，或`Preparing|Unavailable + Idle`；其他cross-product不能构造或编码。

### SessionSnapshot

```rust
pub struct SessionSnapshot {
    pub session_id: SessionId,
    pub lifecycle: SessionLifecycleView,
    pub metadata: SessionMetadataView,
    pub definition: SessionDefinitionSummary,
    pub load_state: SessionLoadStateView,
    pub readiness: SessionReadinessView,
    pub execution: SessionExecutionView,
    pub current_turn: Option<CurrentTurnView>,
    pub active_items: Vec<ItemView>,
    pub pending_interactions: Vec<InteractionView>,
    pub queues: SessionQueueView,
    pub recording: SessionRecordingView,
    pub usage: Option<SessionUsageView>,
    pub diagnostics: Vec<SessionDiagnosticView>,
}

pub enum SessionLifecycleView {
    Open,
    Archived,
    Deleted,
}

pub enum SessionLoadStateView {
    Loaded,
    Unloading,
}

pub enum SessionReadinessView {
    Preparing,
    Ready,
    Unavailable(SessionUnavailableView),
}

pub enum SessionUnavailableView {
    AgentUnavailable,
    WorkspaceUnavailable,
    ModelUnavailable,
    PromptUnavailable,
    DurableStateCorrupt,
    DurableStateTooLarge,
    RuntimeDependencyUnavailable,
}

pub enum SessionExecutionView {
    Idle,
    Starting,
    Running,
    Finishing,
}

pub struct CurrentTurnView {
    pub turn_id: TurnId,
    pub status: TurnStatusView,
    pub phase: Option<TurnExecutionPhaseView>,
    pub started_at: Timestamp,
}

pub enum TurnStatusView {
    Running,
    Completed { completed_at: Timestamp },
    Interrupted {
        completed_at: Timestamp,
        reason: TurnInterruptionView,
    },
    Failed {
        completed_at: Timestamp,
        reason: TurnFailureView,
    },
}

pub enum TurnExecutionPhaseView {
    Sampling,
    RetryBackoff,
    Compacting,
    WaitingApproval,
    WaitingForUserInput,
    ExecutingTools,
}

pub enum TurnInterruptionView {
    UserCancelled,
    SecurityRevoked,
    PrepareForUnload,
    RuntimeShutdown,
    RuntimeFailure,
}

pub enum TurnFailureView {
    Prompt,
    Model,
    Tool,
    ContextOverflow,
    DependencyUnavailable,
    InvariantFailure,
}

pub struct SessionUsageView {
    pub model_calls: u64,
    pub compaction_calls: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reported_costs: Vec<Money>,
}

pub struct SessionQueueView {
    pub submit_admissions: Vec<SubmitAdmissionView>,
    pub steers: Vec<QueuedSteerView>,
    pub follow_ups: Vec<QueuedFollowUpView>,
    pub accepting_input: bool,
}

pub struct SubmitAdmissionView {
    pub command_id: CommandId,
    pub state: SubmitAdmissionStateView,
}

pub enum SubmitAdmissionStateView {
    Queued,
    Starting,
}

pub struct QueuedSteerView {
    pub command_id: CommandId,
    pub expected_turn_id: TurnId,
}

pub struct QueuedFollowUpView {
    pub command_id: CommandId,
}

pub struct SessionRecordingView {
    pub state: SessionRecordingState,
}

#[serde(rename_all = "snake_case")]
pub enum SessionRecordingState {
    Healthy,
    Degraded,
}
```

`reported_costs`按currency升序，每currency最多一个checked aggregate，最多8项。不同currency不能coerce为一个Money；某currency decimal overflow时省略该currency并产生bounded usage diagnostic。若出现超过8种currency，确定性保留lexicographically first 8并产生`usage_currency_limit_exceeded` diagnostic。`model_calls`和`compaction_calls`受JSONL 1,000,000-entry hard cap约束并用checked increment，因此始终可表示；每个optional token aggregate发生u64 overflow时该field变None并产生diagnostic，其他fields仍保留。所有u64 wire使用[ADR 0134](../adr/0134-public-and-conversation-wire-use-bounded-v1-schemas.md) canonical decimal string。

wire固定为object + string state：

```json
{"recording":{"state":"healthy"}}
{"recording":{"state":"degraded"}}
```

长期recorded Turn历史通过Query按需读取。`SessionSnapshot`只存在于已原子发布的loaded execution；Loading尚未发布Session owner，因此`snapshot(Session)`返回SessionNotLoaded/Unavailable而不是半成品Snapshot。Snapshot携带当前loaded Session的live observer baseline；`active_items`完整包含current Turn的live Items，并按live conversation顺序与assistant Reasoning/Text/ToolCall content顺序排列。一个Session最多暴露一个current Turn/ActiveTurnTask，消费[INV-101](../architecture.md#跨模块不变量索引)。Snapshot不是durable checkpoint，也不用于恢复旧Model/Tool waiter。

`SessionRecordingState`语义：

- `Healthy`：当前load尚未观察到record failure；不表示flush、fsync或power-loss durability；
- `Degraded`：初始化、encode或append已经失败；当前load停止后续记录，Session execution继续；
- Create先完成initial SessionHeader staging并发布Unloaded Session；每次Load都尝试初始化Recorder，不提供Disabled、ephemeral Session或per-entry opt-out；
- 同一次load只允许`Healthy → Degraded`；修复storage不会使当前loaded instance恢复；
- 只有显式`Unload + Load`创建的新loaded instance可以重新成为Healthy，并且只恢复recorded prefix；旧unrecorded live tail丢失；
- MVP不提供`ResumeSessionRecording`、`StartRecordingSegment`、自动storage probe/retry或live-tail backfill。

MVP不增加`Disabled`、`Initializing`、`Writing`或per-entry receipt状态。内部`RecordingHealth`可以携带typed reason和failed entry identity，公开view只投影Healthy或Degraded。

recording failure同时产生一条脱敏`SessionDiagnosticView`，code只允许：

```text
session_recording_initialization_failed
session_recording_encode_failed
session_recording_append_failed
session_recording_outcome_unknown
```

raw OS error、绝对路径、credential、完整StoredSessionEntry和Tool output只进入受控内部日志。Session保持`Degraded`期间，latest SessionSnapshot必须保留至少一条当前recording diagnostic，使断线重连的host能够同时恢复状态和粗粒度原因。状态变化与该diagnostic在同一次immutable Snapshot publication中原子可见。

Host展示语义：`Healthy`不提示；`Degraded`持续显示“后续内容无法恢复”的warning，但不得禁用Submit、Model、Tool、Interaction或Compaction。warning应明确：继续使用会保留current live state但不保存；Unload/Load只恢复到最后recorded prefix并丢失之后的live内容。

**Canonical cross-module invariant: INV-103.**

`SessionQueueView`完整列出当前process中每个public cancelable pre-Turn/queued input，Snapshot不得只返回count或截断列表：

- `submit_admissions`按TurnAdmission lane FIFO排列，最多一个entry为`Starting`；其CommandId使用`Cancel(PublicCancelTarget::Submit)`；
- `steers`按current Turn的Steer FIFO排列，携带exact expected TurnId；
- `follow_ups`按FollowUp FIFO排列；
- Steer/FollowUp使用`CancelQueuedMessage(target_command_id)`；
- 三个Vec之间没有cross-lane ordering语义，不能按数组位置或CommandId比较先后；
- queue capacities已经bounded，因此Snapshot必须完整复制lane-local values，不需要分页或preview truncation；
- queued PromptIntent正文、Skill selection、expanded Skill/Workspace content和preview不进入QueueView。host可以恢复类型、目标与合法cancel command，但不能从observer surface读取尚未apply的用户draft；
- `accepting_input`是该Snapshot时点的UI hint，command仍必须重新验证readiness/execution/lane capacity，不能把hint当permit；
- restart后这些process-local Vec为空，host不能把旧CommandId重放为queue事实。

InteractionControl、ToolControl、emergency/lifecycle waiter和SnapshotMailbox深度属于internal diagnostics，不进入普通公开协议。

Cancel被Executor接受后，`SessionExecutionView`必须立即反映`Finishing`并发布`session_execution_changed`。当execution为Finishing时，host优先显示Stopping/Finishing；`CurrentTurnView.phase`保留Cancel前最后工作位置供diagnostic，不建立`TurnExecutionPhase::Cancelling`。Finishing期间host可以继续发送FollowUp并收到`FollowUpQueued`，普通Submit仍返回SessionBusy。

Catalog argument schema只描述safe host input affordance，不包含handler ID、permission object或hidden defaults。`position = Some`的arguments按连续position构造Positional input；`None`使用Named input。Runtime仍必须按current rematerialized catalog重验全部值。Suggestion的`replace`是原input上的half-open UTF-8 byte range，两个端点必须是code-point boundary且满足`start <= end <= input.len()`；host只替换该span，不猜whole-input/token semantics。

### Snapshot Consistency

Runtime与每个Session使用独立owner和Snapshot，不存在跨scope可比较的全局sequence。

`SessionSnapshot`从对应SessionExecutor的latest-wins/coalesced immutable published view读取，不进入mutation/control lane，也不承诺与不同SessionIngress lane形成全局FIFO。`RuntimeSnapshot`由Runtime owner捕获Runtime status与loaded Session membership，不等待所有SessionExecutor parked，也不构造all-loaded stop-the-world barrier；durable Agent/Session catalogs通过paged Query恢复。

单独调用`snapshot()`只读取调用时的当前view。用于持续观察时，host调用`subscribe(scope)`：owner必须在同一publication synchronization内注册subscriber并捕获初始Snapshot，EventStream第一帧返回该Snapshot，随后只发送该点之后的实时事件。禁止用非原子的“先snapshot再subscribe”或“先subscribe再snapshot”替代。

断线、subscriber背压或publisher关闭后，旧stream直接结束；host重新subscribe并从新的首帧Snapshot恢复，不重放缺失事件。Runtime process restart时只从JSONL recorded conversation prefix replay；未record的live tail和旧TurnStatus消失。新Snapshot的`current_turn`为空，不合成recovery terminal。

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

pub enum SubscriptionClosed {
    Backpressure,
    RuntimeClosing,
    PublisherRestarted,
}
```

### StateEvent

```rust
pub struct StateEvent {
    pub timestamp: Timestamp,
    pub command_id: Option<CommandId>,
    pub route: EventRoute,
    pub msg: StateEventMsg,
}

pub enum StateEventMsg {
    Runtime {
        kind: RuntimeStateEventKind,
        snapshot: RuntimeSnapshot,
        detail: Option<RuntimeEventDetail>,
    },
    Session {
        kind: SessionStateEventKind,
        snapshot: Box<SessionSnapshot>,
        detail: Option<SessionEventDetail>,
    },
}

pub enum RuntimeStateEventKind {
    AgentCreated,
    AgentDefinitionUpdated,
    AgentMetadataUpdated,
    AgentStatusChanged,
    SessionCreated,
    SessionLoaded,
    SessionUnloaded,
    SessionDefinitionUpdated,
    SessionMetadataUpdated,
    SessionArchived,
    SessionUnarchived,
    SessionDeleted,
    SessionForked,
    DiagnosticsUpdated,
    SharedResourcesReloaded,
    CommandCatalogInvalidated,
}

pub enum SessionStateEventKind {
    SessionDefinitionUpdated,
    SessionMetadataUpdated,
    SessionWorkspaceReloaded,
    SessionReadinessChanged,
    SessionExecutionChanged,
    SessionSettled,
    TurnStarted,
    TurnPhaseChanged,
    TurnCompleted,
    TurnInterrupted,
    TurnFailed,
    ItemCompleted,
    ItemToolInvocationStarted,
    ItemToolInvocationCompleted,
    ItemToolInvocationAbandoned,
    InteractionRequested,
    InteractionResolved,
    QueueUpdated,
    UsageUpdated,
    SessionRecordingChanged,
    DiagnosticsUpdated,
}

pub enum RuntimeEventDetail {
    AgentChanged {
        agent: AgentSummary,
    },
    SessionChanged {
        session: Box<SessionSummary>,
    },
}

pub enum SessionEventDetail {
    QueueUpdated {
        removed_command_ids: Arc<[CommandId]>,
        reason: QueueUpdateReason,
    },
    ItemChanged {
        item: ItemView,
    },
    TurnTerminal {
        turn_id: TurnId,
        terminal: TurnTerminalView,
    },
    InteractionResolved {
        request_id: RequestId,
        resolution: InteractionResolutionView,
    },
}

pub enum TurnTerminalView {
    Completed { completed_at: Timestamp },
    Interrupted {
        completed_at: Timestamp,
        reason: TurnInterruptionView,
    },
    Failed {
        completed_at: Timestamp,
        reason: TurnFailureView,
    },
}

pub enum QueueUpdateReason {
    CancelQueuedMessage,
    TurnCancelled,
    TurnTerminal,
    PrepareForUnload,
}
```

M2/M11 State codec已materialize Runtime `command_catalog_invalidated | agent_created | agent_definition_updated | agent_metadata_updated | agent_status_changed | session_created | session_loaded | session_unloaded | session_definition_updated | session_metadata_updated | session_archived | session_unarchived | session_deleted | session_forked`以及Session `session_definition_updated | session_metadata_updated | session_execution_changed | turn_completed | turn_interrupted | turn_failed` + matching detail；Progress和Closed EventFrame亦已materialize全部typed payload并通过selected-V1 canonical round-trip。Item/Interaction/queue StateEvent kinds继续返回known `PendingPublicTarget`。`SessionSnapshot`与Runtime `SessionChanged` summary上的Box只控制enum尺寸。

StateEvent本身始终是非durable observer record，不因payload来源不同而成为第二日志：

| 来源 | 例子 | restart后的恢复 |
| --- | --- | --- |
| durable entity owner | Agent/Session definition、metadata、Open/Archived/Deleted | 从entity head/revision恢复；旧StateEvent不回放，conversation JSONL不参与 |
| live domain state | Turn terminal、Item、Interaction request/resolution、active conversation | 当前process立即可见；TurnStatus不恢复，只有recorded conversation facts能在restart后重建 |
| process-local control | load/readiness、execution/phase、queue、settled、recording health、diagnostics | restart后消失、重置或由新状态替代 |

StateEvent规则：

- subscription第一帧必须是与scope匹配的完整Snapshot；
- 同一subscription lifetime内，StateEvent按publisher发送顺序交付；
- subscriber queue无法继续、transport断开或publisher restart时发送Closed或直接终止stream，调用方重新subscribe并从新Snapshot恢复；
- 不缓存StateEvent用于公开replay，也不接受caller-provided offset；
- final domain StateEvent必须从成功live mutation派生。对应recordable conversation fact时，发送前完成inline record attempt；`TurnInterrupted`/`TurnFailed`没有record attempt，等待recordable settlement facts完成后发布；record outcome不提供durable acknowledgement；
- Agent/Session definition与lifecycle StateEvent从对应durable entity mutation派生，不调用SessionRecorder，也不携带conversation EntryId；Runtime-scope durable entity event必须携带matching `RuntimeEventDetail::AgentChanged | SessionChanged` complete safe summary，其中包含current definition/metadata revision与status/lifecycle。Agent/Session metadata使用独立`AgentMetadataUpdated | SessionMetadataUpdated` kind；metadata同样不得写conversation JSONL；
- loaded Session的SessionMetadataUpdated还携带mutation后的SessionSnapshot；unloaded Session没有Session-scope event，但Runtime detail已提供new SessionMetadataRevision。新Runtime subscription仍按RuntimeSnapshot规则重新page catalogs，不依赖旧event replay；
- process-local load/readiness/execution/phase/queue事实必须能从当前Runtime的对应Snapshot读取，但不承诺跨restart恢复；
- `shared_resources_reloaded`和`command_catalog_invalidated`是query invalidation signal，不是独立状态；host收到后重新执行对应safe catalog query。若signal在断线期间丢失，新的Runtime Snapshot本身要求host按需重新query catalogs；
- payload包含完整final view，能够校正之前丢失的ProgressEvent；
- `RuntimeEventDetail::AgentChanged`只用于AgentCreated/AgentDefinitionUpdated/AgentMetadataUpdated/AgentStatusChanged；`SessionChanged`只用于SessionCreated/SessionDefinitionUpdated/SessionMetadataUpdated/SessionArchived/SessionUnarchived/SessionDeleted/SessionForked。detail identity必须匹配EventRoute和kind；
- `ItemCompleted | ItemToolInvocation*`必须携带matching `ItemChanged` detail；
- `TurnCompleted | TurnInterrupted | TurnFailed`必须携带matching `TurnTerminal` detail，即使该event后的Snapshot已经把`current_turn`清空；detail reason只使用safe closed taxonomy，不含provider/Tool/raw error；
- `InteractionResolved`必须携带matching request ID和safe resolution detail；same-key idempotent retry不发布第二event。
- Cancel/PrepareForUnload清理process-local Steer或FollowUp时，`queue_updated`携带被移除的CommandId和typed reason；它只说明队列事实变化，不把未record消息伪造成可恢复UserMessage。

上述kind enum是主要event family的typed schema。每条StateEvent携带该scope mutation后的完整scope Snapshot；Runtime durable catalog mutation额外携带single changed entity summary，SessionEventDetail只承载SessionSnapshot无法表达但对本次transition有用的safe correlation信息。

event semantic labels（ADR 0134 exact snake_case values）：

```text
agent_created
agent_definition_updated
agent_metadata_updated
agent_status_changed
session_created
session_loaded
session_unloaded
session_definition_updated
session_metadata_updated
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
session_recording_changed
diagnostics_updated
shared_resources_reloaded
session_workspace_reloaded
command_catalog_invalidated
```

`shared_resources_reloaded`是Runtime-scope typed unit payload（detail null），表示Prompt/Model两个已实现safe catalog可能变化（Tool/Skill shared roots reload仍pending）；它不携带revision/generation，snapshot为fan-out后的current projection。loaded Session的readiness变化由各自`session_readiness_changed`表达（仅真实变化时发布）。`session_workspace_reloaded { session_id }`是Session-scope payload，表示new WorkspaceSnapshot及Workspace-bound Prompt/Skill captured sources已经一起发布；订阅者随后以new SessionSnapshot为准。

`session_recording_changed`只在loaded Session的公开recording state发生变化时发布。first failure执行`Healthy → Degraded`并在同一Snapshot publication中安装当前脱敏recording diagnostic。重复`record()`在Degraded下返回`NotRecorded`时不重复发布state event。

`session_forked`是Runtime-scope membership invalidation；command caller直接读取`SessionForked.source`，其他subscriber通过`GetSessionForkProvenance`查询durable source，不在Runtime event detail中重复该字段。

failure若发生在TurnStarted、ItemCompleted、InteractionRequested/Resolved或Compaction record path，先发布原domain StateEvent；该event携带的Snapshot已经是Degraded并包含当前diagnostic。随后紧接一次`session_recording_changed`，携带同一recording state。Turn terminal本身不触发recording failure，但会携带当时最新recording state。

MVP固定使用`SessionExecutionChanged | TurnCompleted | TurnInterrupted | TurnFailed`四个已激活的SessionStateEventKind；ADR 0134把它们编码为distinct snake_case wire variants，不得折叠成未定义的`turn_finished`形状。Rust terminal detail仍保持`Completed | Interrupted | Failed` typed union；execution transition只携带完整SessionSnapshot。

### ProgressEvent

```rust
pub struct ProgressEvent {
    pub timestamp: Timestamp,
    pub route: EventRoute,
    pub kind: ProgressEventKind,
    pub update: ProgressUpdate,
}

pub enum ProgressEventKind {
    Model,
    Tool,
    Compaction,
    Retry,
}

pub enum ProgressUpdate {
    ItemStarted {
        item_id: ItemId,
        content_index: u32,
        content_kind: ItemProgressContentKind,
    },
    ItemDelta {
        item_id: ItemId,
        content_index: u32,
        content_kind: ItemProgressContentKind,
        delta: String,
    },
    ToolOutputDelta {
        item_id: ItemId,
        tool_call_id: ToolCallId,
        delta: String,
    },
    ModelRetryScheduled {
        purpose: ModelCallPurpose,
        retry_count: u8,
        ready_at: Timestamp,
    },
    OperationStatus {
        message: String,
    },
}

pub enum ItemProgressContentKind {
    AssistantText,
    Reasoning,
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
```

规则：

- 有stream progress的AgentMessage/Reasoning在首个content update时分配稳定`ItemId`；started、全部delta和最终`item_completed`使用同一ItemId；
- message/reasoning started属于AgentRun ProgressEvent，不创建final live Item；CompactionSummary不创建StreamingItem或ItemId；
- Host漏掉started时可以用首个delta创建临时Item view；漏掉全部progress或provider为non-streaming时直接使用final `item_completed`；
- 可以按SessionId/TurnId/ItemId合并连续delta；
- queue满时可以丢弃中间progress；
- progress缺失不影响StateEvent或Snapshot正确性；
- `item_completed`只在final candidate成功apply到live state并完成inline record attempt后发布，携带完整final Item view；recording failure不阻止发布；
- Cancel/failure丢弃未finalize的streaming state；logical `model_retry_scheduled`要求Host清除上一Model attempt的临时view，若progress丢失则由Turn terminal或新Snapshot最终校正；
- Host收到同ItemId的`item_completed`或Turn terminal后忽略迟到started/delta；
- progress不进入LiveConversation或SessionRecorder；
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

`ItemId`、`EntryId`、`ToolCallId`、timestamp、Tool completion time和ProgressEvent arrival都不能用于推断current Item顺序。`SessionSnapshot.active_items`按canonical Item order返回；Host必须保留Vec顺序。未来turn-scoped `ListItems`若实现也必须复用同一排序语义。

新live Item的StateEvent按canonical创建顺序发布并追加到live Item list。ToolInvocation的Completed/Abandoned事件和Tool progress都只按`ItemId`更新原位置，不移动Item；因此Tool B可以先结束，展示位置仍保持call order A、B。

AgentMessage/Reasoning progress只属于独立provisional view。收到matching new-Item final StateEvent时Host删除provisional view，并按该创建事件顺序加入live list；Snapshot整体替换live list并清空provisional view。未知Item的terminal update表示reducer失步，Host应重新subscribe获取Snapshot。

### Interaction Event

request-before-notify与resolution-before-resume的live顺序由[INV-301](../architecture.md#跨模块不变量索引)定义；Runtime Interface只冻结UI-safe view和host提交resolution所需route。

`interaction_requested`必须携带UI-safe request：

```rust
pub struct InteractionView {
    pub request_id: RequestId,
    pub turn_id: TurnId,
    pub item_id: ItemId,
    pub request: InteractionRequestView,
}

pub enum InteractionRequestView {
    ToolApproval(ToolApprovalRequestView),
    UserQuestion(UserQuestionRequest),
}

pub struct InteractionResolutionView {
    // owner-private safe projection; no public enum discriminant
}
```

`InteractionResolutionView`是Turn/Interaction owner提供的safe opaque projection，不是Runtime-local enum or shadow DTO；owner以narrow safe view/ref access表达ToolApproval、UserAnswer或Cancelled。prepared Tool args、executor handle、private option→PermissionSet map、sandbox internals和credential不进入event。`pending_interactions`只含Pending request；resolved request从Snapshot移除，并通过`InteractionResolved` event detail传递safe resolution。Host回答时必须回传SessionId、expected TurnId、ItemId、RequestId和resolution key。UI可以执行本地required-field/choice校验，但MiniCore仍要校验resolution family、identity、option/question indices和first-wins状态。

UserQuestion request/answer是non-secret recordable data；它们可以进入event/history/ToolResult。Presentation Adapter必须提示不能输入credential。Runtime没有`secret` field，Transport Adapter也不能私自把某个Text answer当作secure channel。

## Message Tree 管理

Session message/history tree由MiniCore内部管理：

```text
LiveSessionState
├─ owns Session-scoped EntryIdGenerator, full immutable selected entry path, and its derived current head
├─ validates domain mutation and binds EntryId + parent_id before apply
└─ preserves identity when recording is Degraded

SessionRecorder
└─ validates immutable entry identity/relation, then encode/append without rewriting ID

ConversationStorage
├─ owns recorded EntryId + parent_id tree, tolerant replay and fork staging
├─ isolates invalid replay references and emits diagnostics
└─ produces recorded history/read projections

SessionExecutor / ActiveTurnTask
├─ invokes LiveSessionState private mutation methods
├─ owns active Turn mutation ordering
└─ requests ConversationStorage history/fork operations

MiniCoreRuntime
├─ exposes library-only session_transcript for loaded basic User/Assistant history
├─ exposes SessionQuery::GetSessionForkProvenance
└─ exposes SessionCommand::Fork
```

UI不能：

- 直接读取或修改JSONL；
- 直接指定任意EntryId作为recorded parent或要求Recorder分配/改写ID；
- 追加raw message；
- 补写ToolResult、猜测parent或重写中段损坏entry；
- 删除历史entry；
- 修改current leaf pointer。

公开能力：

- v0.1在Session loaded后分页读取selected history中的基础User/Assistant文本；current Turn仍从Snapshot/Event读取；
- 使用Genesis、UserMessage或FinalAgentMessage anchor创建Fork Session；
- 查询fork provenance；
- 在新Session继续future Turn。

完整compact history tree、按TurnId分组的Tool/Interaction/Compaction read model、unloaded browsing、search/export与same-Session checkout均为post-MVP。

同一Session原地navigation/checkout需要额外定义head mutation、active Turn conflict、compaction overlay和event语义，留到出现真实产品需求后设计。

## Agent 与 Session 管理

### Agent

```rust
pub struct AgentMetadataView {
    pub revision: AgentMetadataRevision,
    pub name: String,
    pub description: Option<String>,
    pub updated_at: Timestamp,
}

pub struct AgentSummary {
    pub agent_id: AgentId,
    pub definition_revision: AgentRevision,
    pub metadata: AgentMetadataView,
    pub status: AgentStatus,
    pub created_at: Timestamp,
}
```

`ListAgents`和`GetAgent`都返回包含matching metadata revision的safe head projection；`GetAgentRevision`另行返回immutable execution definition。MiniCoreRuntime路由Agent command到durable Agent owner：

```text
Create/Update/Status/Delete
→ Agent lifecycle synchronization
→ durable head/revision publication
→ Runtime StateEvent
```

Session保存exact AgentRevisionRef。Agent发布新revision不会自动改变existing Session。

### Session

```rust
pub struct SessionMetadataView {
    pub revision: SessionMetadataRevision,
    pub name: Option<String>,
    pub description: Option<String>,
    pub updated_at: Timestamp,
}

pub struct SessionSummary {
    pub session_id: SessionId,
    pub definition_revision: SessionDefinitionRevision,
    pub metadata: SessionMetadataView,
    pub lifecycle: SessionLifecycleView,
    pub forked: bool,
    pub created_at: Timestamp,
}
```

`ListSessions`和`GetSession`从durable Session owner返回该projection，不要求Session loaded。definition revision与metadata revision正交；host更新metadata时只回传`metadata.revision`。MiniCoreRuntime维护：

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
- unload先设置`PrepareForUnload` lifecycle signal：同步关闭admission gate、清理queued Steer/FollowUp（无queue_updated event，queue只经subsequent snapshots/terminal event体现），在有限grace期（default 30s、≤5min）内让active admission/Turn自然完成，deadline到期后对exact current emergency target signal `PrepareForUnload`（sticky first-wins，更早Cancel/SecurityRevoked保留原reason）并fail-closed cancel且以`SessionUnloaded` settle pending Interaction，再移除loaded owner；grace期内Submit公开映射`SessionNotLoaded`（registry本身closing才映射`RuntimeClosing`）；
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
→ SessionExecutor installs Starting candidate/Submit emergency target
→ WorkspaceResolver.resolve()
→ shared publication gate克隆PromptResourceView / SkillResourceView / ToolResourceView / ModelCatalogView
→ ModelGateway.resolve_for_turn(captured catalog)
→ create Arc<SkillViewContext>
→ SkillService.for_turn(captured resources, context.clone())
→ ToolService.for_turn(captured tool resources)
→ PromptService.for_turn()
→ TurnExecutionContext
→ await TurnExecutionContext.resolve_user_message()（内部同步调用PromptSet.compose_user_message）
→ revalidate candidate/control/emergency/authority basis
→ LiveSessionState.apply(UserMessage source=Input + Turn Running)
→ await SessionRecorder.record
→ CommandResponse::TurnStarted { turn_id }
→ ActiveTurnTask async loop
→ PromptSet.assemble(LiveConversationView)
→ AssembledModelContext
→ ModelCallRequest
→ ModelGateway.generate_model_turn()
→ ModelGateway private ProviderAdapter（production使用OpenAI Responses或Anthropic Messages protocol-specific adapter，仅处理provider attempt）
→ OpenAI Responses / Anthropic Messages / other provider
→ ModelCallResult
→ ActiveTurnTask validates SessionId + TurnId + control_generation + ConversationRevision
→ apply live recordable AgentMessage/Tool result + await inline record attempt
→ apply/publish live Turn terminal without terminal entry
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
→ CommandCompletion::Completed {
     outcome = SessionDefinitionUpdated { definition_revision },
     output = optional bounded plain text
   }
→ session_definition_updated StateEvent
→ future Turn admission resolve new TurnModelSnapshot
```

### Tool Approval

```text
ModelCallResult contains ToolCall
→ ActiveTurnTask creates live ToolInvocation ItemId
→ apply InteractionRequested live + await inline record attempt
→ session StateEvent::InteractionRequested
→ UI dispatch InteractionCommand::Resolve(
     ToolApproval::Allow { option_index } | Deny,
     fresh/reused InteractionResolutionKey
   )
→ MiniCore validates exact pending request option/family/key
→ apply InteractionResolved live + await inline record attempt
→ session StateEvent::InteractionResolved
→ ActiveTurnTask owner-local ToolStartPermit
→ executor side effect
→ apply truthful Tool result live + await inline record attempt
→ live reducer forms complete Tool exchange
→ next PromptSet.assemble
```

### User Question

```text
ModelCallResult contains built-in ask-user ToolCall
→ ActiveTurnTask creates live ToolInvocation ItemId
→ ToolExecutionControl.request_user_question
→ apply InteractionRequested(UserQuestion) live + await inline record attempt
→ session StateEvent::InteractionRequested with UI-safe InteractionView
→ Presentation Adapter展示non-secret Text/SingleChoice fields并明确禁止credential输入
→ UI dispatch InteractionCommand::Resolve(
     UserAnswer,
     fresh/reused InteractionResolutionKey
   )
→ MiniCore validates required fields/indices/family/key
→ apply InteractionResolved live + await inline record attempt
→ session StateEvent::InteractionResolved
→ wake original Tool future
→ apply PreExecution truthful Tool result live + await inline record attempt
→ complete exchange后same Turn next PromptSet.assemble
```

Production builtin（ADR 0142）：ToolName恰为`ask_user`，closed input schema与`MiniCoreRuntimeConfig::with_ask_user_tool()` default-off idempotent opt-in在[Tools](tools.md#production-ask-user-builtin)冻结；builtin绝不创建executor/start factory/approval，也不reserve/start per-slot gate，answer binding先经exact `validate_answer`验证再产生`PreExecution + Succeeded`、恰一个deterministic compact JSON Text part（`{"answers":[...]}`，ascending order，optional未答`{"answers":[]}`），render/invariant失败fail closed为Abandoned RuntimeFailure。host通过既有`InteractionCommand::Resolve`返回`UserAnswer`即可，无需新增Runtime capability。

Production basic builtins：`with_read_file_tool()`、`with_list_directory_tool()`、`with_write_file_tool()`与`with_fetch_url_tool()`均default-off/idempotent，并与`with_ask_user_tool()`独立。`open`把`fetch_url` selection与`with_fetch_url_origin(...)` installations fail-closed汇合后，每次Turn admission按固定顺序`ask_user → read_file → list_directory → write_file → fetch_url` materialize selection；outer sandbox是enabled routes所需的exact `FilesystemRead`/`FilesystemWrite`/`Network` union。read/list-only使用ReadOnly authority ceiling；write开启时使用ReadWrite ceiling但requested ReadOnly仍保持ReadOnly。`read_file`是bounded UTF-8 regular-file read；`list_directory`是bounded direct enumeration。ADR 0146的`write_file`是cwd-relative safe UTF-8 full replacement/create（path≤4,096 bytes、content≤16,384 bytes），不mkdir/append/atomic rename/fsync；它以opaque capability-prepared target产生physical mutation key，same-Session same-target calls按call_index FIFO，permit由ToolOperationSlot持有through Settling。preparation与write分别是owner-tracked blocking job；scheduled后等待同一job并保持truthful result。任一filesystem opt-in的per-Session永久revocation先revoke再signal/re-resolve，subsequent file/directory reads与writes共同Denied。ADR 0147的`fetch_url`只允许host-installed exact HTTPS DNS origin及其1..=8 pinned addresses：无ambient DNS/redirect/retry/proxy/compression，发送一个fixed-header GET，只将2xx、≤65,536-byte、allowed media type的safe UTF-8 body作为single Text返回；start后cancel在same owner future内drop exact send/body state。Tool opt-in但zero authority、selected duplicate canonical origins均使`open`失败；origin-only config不披露Tool。这些opt-ins不新增wire command/capability。

Pending期间`TurnStatus`和`SessionExecutionState`仍为Running，`TurnExecutionPhase = WaitingForUserInput`。ActiveTurnTask只等待oneshot，对应SessionExecutor control actor继续处理Resolve/Cancel/SecurityRevoked/Unload/Snapshot；其他Session不受影响。等待期间不预留file mutation ticket，也不持有ToolStartPermit或reserve/start per-slot ToolStartGate，elapsed time不会自动关闭Interaction。

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
- model logical retry policy；
- storage truth。

Presentation Adapter与Transport Adapter可以由同一个host实现，但职责不同：Presentation Adapter决定modal、聊天卡片、终端菜单、表单文案与本地交互；Transport Adapter只传输Runtime已定义的request/resolution。两者都不拥有MiniCore Interaction truth。

Transport request id不能替代CommandId、TurnId或RequestId。

## Version 与 Capability

ProtocolHello、ProtocolWelcome、ProtocolReject、version selection、capability token和nested ProtocolLimits的唯一wire shape位于[Wire Schema · Public Protocol V1.0](wire-schema.md#public-protocol-v10)。Runtime Interface只拥有capability的业务可用性，不复制第二套bootstrap DTO或serde规则。

MVP exact version为`1.0`，capabilities为：

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

- adapter按Wire Schema选择highest mutually supported exact version，无交集typed reject，不silent downgrade；
- Runtime只发送selected minor声明的fields/variants；client defensively忽略unknown output field，但unknown variant是protocol error；
- optional feature必须通过capability intersection；
- adapter不能根据runtime implementation version字符串猜测字段；
- experimental capability不进入stable default surface。

Core in-process interface通过`RuntimeQuery::GetCapabilities`读取同一能力视图，不额外要求transport handshake。

## Security 与 Redaction

公开协议不得包含：

- API key、OAuth token、auth header；
- provider endpoint secret和raw response body；
- PromptSet完整System sections和前置User context；
- Skill正文和PromptDefinition正文；
- Tool executor handle、prepared private args和sandbox internals；
- WorkspaceSnapshot、EmergencyControl signal或security target；
- SessionRecorder file/health internals、replay internals；
- raw JSONL path作为普通UI能力；
- internal handler id和完整RuntimeCommand嵌入catalog action。

Renderer不应直接持有MiniCoreRuntime credential或filesystem capability。Tauri/Vue架构中，可信backend调用Runtime，renderer只消费serializable safe view并提交受限Command。

## 内部对象禁止公开

以下类型永远留在crate内部：

```text
SessionExecutor
SessionExecutionHandle
SessionIngress及其内部lane
ActiveTurnTask / ActiveTurnHandle / ActiveTurnControl
ToolStartGate / LifecycleControl / EmergencyControl
ConversationRevision / control_generation
TurnExecutionContext
PromptService / PromptSet
ToolService / ToolSet
SkillService / SkillView / LoadedSkill
WorkspaceSnapshot / EmergencyControl signal
ModelGateway / TurnModelSnapshot private execution ref
ProviderAdapter
SessionRecorder / ConversationStorage implementation
LiveSessionState / LiveConversation reducer
ToolExecutionControl / ToolStartPermit
```

公开view可以投影其安全状态，但不能携带内部handle或允许调用方拼接不一致快照。

`CredentialSource`和`ProviderCredential`是public host-only runtime configuration类型，但它们从不进入public snapshots/events/commands/Wire或renderer-safe views；resolved credentials保持private，只属于单次provider attempt。No public surface change：上述internal-objects列表只禁止private runtime machinery（如`ProviderAdapter`）进入public surface，host-only configuration seam不是public runtime surface。

## Error 分层

```text
RuntimeDispatchError
  接纳前请求无法进入Runtime，或唯一的post-admission integrity-fatal `InternalDispatchUnavailable`（DurableState poison / required live-publication poison）；later closed requests use RuntimeClosed

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

### Command Error Mapping

Runtime Interface只拥有public projection，不把module error迁移到全局error hierarchy。canonical映射如下：

| Internal condition | Public code | RetryAdvice |
| --- | --- | --- |
| accepted typed command的semantic shape/value invalid、duplicate SkillId、`PromptErrorKind::InvalidIntent`或`InvalidContribution` | `InvalidArgument` | `DoNotRetry` |
| missing Agent/Session/Skill/Item/Request | `NotFound` | `RefreshAndRetry` |
| same CommandId或Interaction resolution key携带different canonical payload | `CommandConflict` | `DoNotRetry` |
| Agent/Session definition或metadata CAS mismatch | `StaleRevision` | `RefreshAndRetry` |
| Agent disabled / deleted | `AgentDisabled` / `AgentDeleted` | `UserActionRequired` / `DoNotRetry` |
| Session archived / deleted | `SessionArchived` / `SessionDeleted` | `UserActionRequired` / `DoNotRetry` |
| loaded executor不存在 | `SessionNotLoaded` | `UserActionRequired` |
| loaded Session readiness为Preparing或Unavailable且没有更具体public code | `SessionNotReady` | 按下表exact cause |
| Starting/Running/Finishing conflict | `SessionBusy` | `RefreshAndRetry` |
| reload candidate validation失败 | `ReloadValidationFailed` | `UserActionRequired` |
| bounded public work lane满 | `IngressLaneFull { lane }` | `RetryWithBackoff` |
| Steer/FollowUp已消费或不在queue | `QueuedMessageNotQueued` | `RefreshAndRetry` |
| Submit target已发布Turn或不再处于pre-Turn cancel window | `SubmitNotCancellable` | `RefreshAndRetry` |
| expected Turn与current不同 | `ExpectedTurnMismatch` | `RefreshAndRetry` |
| Turn absent / cancelling / terminal | `TurnNotRunning`、`TurnCancelling`或`TurnTerminal` | `RefreshAndRetry` |
| Interaction absent / already terminal | `InteractionNotFound`或`InteractionAlreadyResolved` | `RefreshAndRetry` |
| Interaction request/resolution family mismatch | `InteractionFamilyMismatch` | `DoNotRetry` |
| stale/cross-session/illegal Fork anchor | `InvalidForkAnchor` | `RefreshAndRetry` |
| Workspace authority或security policy拒绝 | `Unauthorized` | `UserActionRequired` |
| temporary source/storage/model resolution unavailable before admission | `Unavailable` | `RetryWithBackoff` |
| required durable entity/history损坏 | `DurableStateCorrupt` | `UserActionRequired` |
| valid durable history超过v1 1 GiB/1,000,000-entry hard cap | `DurableStateTooLarge` | `UserActionRequired` |
| request rejected before its relevant named barrier because Runtime is Closing | `RuntimeClosing` | `RetryWithBackoff` |
| internal invariant/channel failure发生在command seam | `Unavailable` | `DoNotRetry` + redacted diagnostic |

Envelope/runtime入口失败不进入上述Command table：

| Pre-command condition | Outer result |
| --- | --- |
| 无法decode/validate `CommandRequest` envelope | `RuntimeDispatchError::InvalidEnvelope` |
| frame/request超过outer transport/runtime request limit | `RuntimeDispatchError::RequestTooLarge` |
| Runtime已经closed | `RuntimeDispatchError::RuntimeClosed` |
| command dedup/router在接纳前不可用，或joined post-admission waiters观察到DurableState poison / post-commit required live-publication invariant poison | `RuntimeDispatchError::InternalDispatchUnavailable` |

一旦CommandId与typed RuntimeCommand已经被dedup owner接纳，后续field/domain size或semantic validation只能完成为`CommandCompletion::Rejected(CommandError)`，不能再返回outer error。

`SessionNotReady`的RetryAdvice固定映射：

| Readiness cause | RetryAdvice / override |
| --- | --- |
| `Preparing` | `RetryWithBackoff` |
| `Unavailable(AgentUnavailable)` | `UserActionRequired` |
| `Unavailable(WorkspaceUnavailable)` | `UserActionRequired` |
| `Unavailable(ModelUnavailable)` | `UserActionRequired` |
| `Unavailable(PromptUnavailable)` | `UserActionRequired` |
| `Unavailable(RuntimeDependencyUnavailable)` | `RetryWithBackoff` |
| `Unavailable(DurableStateCorrupt)` | override code为`DurableStateCorrupt` + `UserActionRequired`，不使用`SessionNotReady` |
| `Unavailable(DurableStateTooLarge)` | override code为`DurableStateTooLarge` + `UserActionRequired`，不使用`SessionNotReady` |

`Unavailable(RuntimeDependencyUnavailable)`的唯一真实producer是loaded Turn admission读取pinned historical AgentRevisionRef时`DurableState::read_agent_definition`的transient `StorageUnavailable`；它不是host global bool，也不是`ReloadSharedResources`（shared-resource reload只替换Prompt/Model roots、不触碰本fact），open-time Tokio `RuntimeDependencyUnavailable`仍只属于`RuntimeInitializationError`。首次失败settle回Idle后安装独立`runtime_dependency_unavailable` fact并发布`ReadinessChanged(command_id None)`，Submit按上表返回`SessionNotReady`+`RetryWithBackoff`并立即启动owner-tracked无TurnId probe（复用同一exact read路径）；probe仍Unavailable则等next Submit re-arm，Recovered清fact、发布`ReadinessChanged(command_id None)`并保留retained FollowUp handoff。admission直接观察到AgentNotFound/RevisionUnavailable时分类为`AgentUnavailable`而非本cause；fact安装后的probe若发现同一retained ref消失则是durable invariant并进入internal/fatal，model recapture failure同样是internal invariant，fatal/closing/corrupt/too-large不进入本cause；恢复只由exact DurableState read probe与Submit re-arm拥有，无新public/wire command，manifest现为144项；RuntimeDependencyUnavailable真实historical storage fault+probe/rearm+retained FollowUp fixtures/tests已补齐（scenario/fixture closure已实现，统一质量门禁已通过）。

stage规则：

- Prompt/Skill/Workspace/Agent/model resolution发生在initiating Input live apply前时，可以`Rejected(CommandError)`；Input没有部分apply；
- user Cancel在Input apply前获胜是`Completed(SubmitCancelled)`，不是`CommandErrorCode`；
- Input live apply后原Submit已经成功，后续Prompt assembly、Model、Compaction或ordinary loop failure只能发布`TurnFailed` StateEvent/Snapshot，不得retroactively Reject Submit；若shutdown在Recorder physical barrier前先赢，record attempt truthful返回`NotRecorded`/Degraded，owner仍完成`TurnStarted`并随后settle同一Turn interruption。只有post-commit required live-publication integrity poison使用outer `InternalDispatchUnavailable`；
- Tool unknown/schema/policy/approval/sandbox/cancel-before-start形成truthful PreExecution ToolResult；Running后outcome unknown形成Abandoned。它们不是CommandError；
- Interaction `Resolve`自身的route/family/idempotency错误仍是CommandError；被接受后Tool后续失败走ToolResult/Turn terminal；
- SessionRecorder failure只把recording投影为Degraded，不改变command outcome、Turn terminal reason或retry advice；
- ModelGateway error永不直接公开为CommandError。TurnStarted后的Model failure由TurnFailed safe reason/diagnostic投影，provider raw message不越界。

Session Execution、Prompt、Tools、ModelGateway和Storage保留各自typed error owner；禁止新增通用`RecoverableError` trait、global error registry或severity系统。

## Test Strategy

Public interface是 contract test surface。

### Command Tests

- Agent/Session definition revision与metadata revision独立CAS；Create同时返回两个revision 1；
- Agent/Session create/definition/metadata DTO不接受owner-assigned ID/revision/timestamp；OptionalTextPatch正确区分Keep/Set/Clear，empty/equivalent patch在CAS后NoChange；
- pre-command public `WorkspaceDefinitionInput::new`按V1 hard maxima拒绝duplicate root key/URI、unknown cwd root、cwd escape和oversized roots；Wire crate-internal constructor额外消费selected effective limits；两者失败时都没有typed command或WorkspaceRevision；
- admitted Create/Update使用显式`WorkspacePathTarget`执行host lowering：supported family成功形成private `WorkspaceRootSpec { path: PathBuf }`，unsupported family完成为`Rejected(InvalidArgument + DoNotRetry)`；只有lowering成功后才分配new WorkspaceRevision；
- ExecuteCatalog selection只用safe path/ordered typed args，Text/Boolean/Choice必须匹配current schema，Runtime重验且不能通过catalog注入RuntimeCommand/credential；
- metadata stale expected token失败，canonical no-op保持token且不发布event，successful update返回new metadata revision；
- Agent/Session metadata update不写conversation JSONL，archive/delete竞态按entity lifecycle synchronization线性化；
- Create Session只有在SessionDefinition与SessionHeader staging成功后返回SessionCreated；失败不发布partial Session；
- Load始终尝试初始化Recorder，初始化失败后SessionSnapshot recording为Degraded；
- Submit只在UserMessage成功live apply并完成inline record attempt后返回TurnStarted；
- duplicate in-flight Submit使用相同CommandId加入同一completion，不创建第二个candidate；
- user Cancel在Input apply前使所有joined Submit caller得到同一SubmitCancelled completion；Input apply先赢时全部得到同一TurnStarted；
- 相同CommandId携带不同command返回Rejected CommandError(code=CommandConflict)；
- invalid envelope/request-too-large/closed runtime使用outer RuntimeDispatchError；accepted typed command的field/domain invalid保留CommandId进入CommandCompletion::Rejected(InvalidArgument)；
- SessionNotReady每个Preparing/Unavailable cause按canonical table产生exact retry，DurableStateCorrupt/TooLarge使用专用code；
- CommandOutput只允许bounded redacted plain text；
- Cancel(Submit CommandId)关闭排队或Starting admission；Input apply前user Cancel使原Submit完成SubmitCancelled且无Turn，Input已live apply但TurnStarted尚未发布时原Submit仍完成TurnStarted并阻止task spawn；target退休或restart后返回NotFound，已发布Turn target返回SubmitNotCancellable且不影响future Turn；
- Starting async Skill load期间user Cancel使原Submit为SubmitCancelled；SecurityRevoked使原Submit Rejected(Unauthorized)；两者在Input apply前先赢时均无Turn/task，apply后保持TurnStarted→Interrupted顺序；
- Steer expected TurnId与per-Turn FIFO Queued；CancelQueuedMessage只remove一条/NotQueued；
- FollowUp bounded queue和restart loss；
- 普通work lane满时Cancel仍可进入EmergencyControl并触发取消；
- valid Cancel立即返回`CancelAccepted`且不等待Tool settlement；duplicate返回同一target/epoch；
- Cancel在Input live apply前取消candidate，apply后绑定同一Turn；cancel epoch与live Completed decision first-wins，不能accepted后再提交Completed；
- Cancel清理current Turn Steer但保留FollowUp，Finishing期间新FollowUp仍可Queued；
- CancelAccepted后execution snapshot/event进入Finishing，最终TurnInterrupted另行发布；
- stale Cancel TurnId不取消新的active Turn；
- Unload grace deadline到期后signal `PrepareForUnload`并Cancel active Turn，以`SessionUnloaded`关闭Interaction（更早Cancel/SecurityRevoked first-wins保留原reason：`TurnCancelled`/`SecurityRevoked`）；
- Interaction长时间无回答或subscriber断开时保持Pending，不产生默认Deny；
- Interaction first live terminal resolution wins；same key/same canonical payload幂等且无第二record/event，same key/different payload为CommandConflict，different key after terminal为InteractionAlreadyResolved；
- Tool approval只接受exact request option index或Deny；unknown index/cross-request reuse拒绝，restricted option不能扩大PermissionSet；
- UserQuestion Text/SingleChoice required/index/family/total-size validation；protocol不存在secret/password answer variant；
- Fork Before/After Item anchor；
- loaded Fork outcome/provenance为LiveSnapshot并包含capture前已apply的unrecorded tail；
- unloaded Fork outcome/provenance为RecordedHistory；
- Fork与Unload竞态的source kind和复制path来自同一linearization decision；
- live Fork staging失败不发布SessionForked或partial child；
- `/model`和`/thinking`生成new SessionDefinitionRevision；
- `/skill code-review task`解析为Text body加一个SkillId selection，不产生Skill/Composite/Template variant；
- Template body variant在MVP不可构造/不可decode；future template需协商new capability和完整typed contract；
- duplicate SkillId在边界拒绝，exact captured Skill失败不apply部分UserMessage；
- Submit与Steer共享async captured Skill resolve；reload期间Steer继续使用old bytes，resolve await不持有ordinary state guard；
- TurnStarted后的Prompt/Model/Compaction failure发布TurnFailed而不改变原Submit completion；Tool failure走truthful ToolResult/Abandoned，Recorder failure只改变recording health；
- public error mapping每个code/retry/subject family有contract vector，host不解析message；
- 用户显式Skill选择不创建Item，模型Skill Tool仍创建ToolInvocation Item；
- `/compact`不在catalog。

### Query/Snapshot Tests

- Query只读且不发布Event；
- 每个concrete nested Query variant只返回matching QueryResult variant，family mismatch触发invariant failure而非空payload；
- PageRequest limit、stale/cross-family/cross-snapshot cursor、canonical sort、UTF-8 command cursor boundary和suggestion half-open replacement range；
- Query/Snapshot/Subscription closed error code、retry和subject vectors；
- QueryResponse所有family使用closed QueryResult variant，domain CAS token内联在对应summary/definition，不存在generic QueryRevision；
- session list不加载SessionExecutor；
- library-only transcript first page要求Session loaded，continuation绑定same immutable capture与Session；
- transcript continuation在Unload后仍可读，fresh read返回SessionNotLoaded，cross-Session/consumed cursor返回StaleCursor；
- transcript排除current Turn、Reasoning/Tool/Interaction/Compaction，只按selected path投影基础User/Assistant safe text；
- normal shutdown/reopen/Load后只恢复recorded prefix，ModelUnavailable不阻止transcript read；
- transcript/capture Debug不暴露正文；
- post-MVP `GetHistoryTree/ListTurns/GetTurn/ListItems`若实现，再增加完整branch topology、historical grouping与unloaded read gates；
- `SessionSnapshot.active_items`保持current Turn canonical Item顺序且不分页；
- SessionSnapshot读取immutable published view，active_items保持canonical Item顺序；
- first/new SessionSnapshot完整枚举Submit admission、Steer和FollowUp CommandId，lane-local顺序稳定且不公开prompt preview；
- Snapshot中每个Submit entry可用Cancel(Submit)定位，每个Steer/FollowUp可用CancelQueuedMessage定位；Starting转TurnStarted后Submit CommandId消失并由current TurnId成为cancel target；
- RuntimeSnapshot不等待所有SessionExecutor；每个new Runtime Snapshot后host必须重新分页ListAgents/ListSessions，恢复断线期间durable catalog变化；
- pending Interaction可从SessionSnapshot恢复，request足以完整渲染并构造合法Resolve；private approval option map不进入Snapshot；
- usage replay按currency分组、mixed currency不合并、decimal/u64 overflow产生bounded diagnostic且不wrap；
- Healthy/Degraded两态JSON wire稳定，Create无recording opt-out且每次Load都尝试初始化Recorder；
- Degraded Snapshot始终携带至少一条当前脱敏recording diagnostic；
- snapshot-first subscription的首帧Snapshot与后续事件无缺口、无旧事件回放。

### Event Tests

- Runtime与每个Session subscription彼此独立；
- AgentMetadataUpdated/SessionMetadataUpdated使用独立event kind；Runtime event detail携带matching complete AgentSummary/SessionSummary和new CAS token，loaded Session event还携带new SessionSnapshot；unloaded Session metadata只发布Runtime-scope event；
- all durable Agent/Session catalog event kind与RuntimeEventDetail family/identity匹配，RuntimeSnapshot仍不内联全catalog；
- 每条stream首帧是scope匹配的Snapshot；
- 当前subscription内StateEvent保持发送顺序；
- subscriber背压、disconnect和restart关闭stream，重新subscribe返回新Snapshot；
- restart后只从recorded prefix重建，unrecorded live tail、queued Submit/Steer/FollowUp和旧phase不恢复；
- ProgressEvent可以合并/丢弃；
- message/reasoning started、delta和completed使用稳定ItemId，丢失started/delta仍可由completed校正；
- live final mutation失败不发布item_completed；recording失败仍可发布；logical retry、Turn terminal或新Snapshot清理临时Item view；
- Healthy首次失败时原domain event先携带Degraded Snapshot发布，随后一次`session_recording_changed`；后续NotRecorded不重复发state event；
- public wire不接受Disabled，Create/Load没有recording opt-out字段；Create header staging失败不发布Session，Load recorder初始化失败投影为Degraded；
- Degraded后修复storage、重复Load command或后续record call均不能恢复当前loaded instance；
- Unload/Load的新Snapshot可以重新Healthy，但只包含recorded prefix；
- logical model_retry_scheduled丢失时不影响最终Snapshot/terminal校正；
- final Item event携带完整ItemChanged detail和UI-safe ItemView；
- assistant content创建事件按Reasoning/Text/ToolCall顺序发布；
- parallel Tool逆序完成只更新各自原位置，不改变call order；
- Snapshot替换有序live Items并清空provisional Items；
- Runtime restart从JSONL tolerant replay开始，局部坏行/orphan/不完整Tool exchange通过diagnostic呈现；旧TurnStatus不恢复，也不把Snapshot当作execution checkpoint；
- Interaction request live apply/record-attempt-before-notify和resolution live apply/record-attempt-before-resume；
- InteractionResolved携带safe resolution detail，包括closed cancellation reason；same-key idempotent retry不发布第二event；
- elapsed time和subscriber缺失不产生Interaction resolution；
- UserQuestion event只携带non-secret Text/SingleChoice request；UI提交validated UserAnswer后恢复同一Turn而不是创建UserMessage；
- Pending UserQuestion可由SessionSnapshot重建展示，Session A等待不影响Session B事件推进；
- Turn只有一个terminal event，且携带matching TurnTerminal detail；event后的Snapshot可以清空current_turn；
- subscriber buffer不足时关闭stream，不做event replay。

### Security Tests

- catalog/query/snapshot/event不包含credential；
- SessionDefinitionSummary不包含Workspace absolute path；ItemView不包含Skill/Workspace注入正文、raw Tool args/result或hidden reasoning；
- renderer不能提交raw internal command；
- Runtime没有generic command/query map、arbitrary JSON args或未定义nested public request variant；
- renderer不能直接持有Tool waiter、SessionRecorder或伪造MiniCore未请求的Pending Interaction；
- approval view不包含raw arguments/private PermissionSet map；unknown option不能扩大权限；
- UserQuestion request/answer没有secret variant，credential/password/token不得进入Interaction/JSONL/ToolResult/model；
- QueueView只暴露cancel所需CommandId/kind/Turn target，不包含queued PromptIntent正文、Skill IDs或preview；
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

缺点：streaming/progress、model logical retry和UI状态会污染Session history；observer delivery failure也不能决定conversation durability。

结论：不采用。LiveSessionState驱动当前Runtime和observer；SessionRecorder保存best-effort resume history。StateEvent与recorded JSONL都不互相充当传输或durability acknowledgement。

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

- [x] 确定MiniCoreRuntime四个Wire-compatible families及明确标注的library-only/host-only seams。
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
