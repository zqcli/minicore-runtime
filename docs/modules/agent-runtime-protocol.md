# AgentRuntimeProtocol

`AgentRuntimeProtocol` 是下游 CLI/TUI/GUI adapter 与 `AgentRuntime` 之间的稳定通信协议模块，包含命令、领域分组查询、事件、快照和共享视图类型。下游宿主只应依赖这个协议，不依赖 Rig、工具实现或 session 文件。

事件命名、顺序、commit 后领域事实、重连和各场景生命周期详见 [AgentRuntimeEvents](agent-runtime-events.md)。本文件只保留 UI 需要依赖的协议类型。

Rust 模块名建议使用 `agent_runtime_protocol`。模块内对外命令枚举使用 `AgentCommand`，表示“下游 adapter 发给 `AgentRuntime` 的协议级用户意图”。`Command` 这个短名留给 [CommandSurface](command-surface.md) 中的 session-scoped `command::Command` 子系统入口。跨模块引用时使用完整路径，例如 `agent_runtime_protocol::AgentCommand`。

## 公共 Interface

```rust
use crate::agent_runtime_protocol as protocol;

pub trait AgentRuntime {
    async fn dispatch(&self, command: protocol::AgentCommand) -> Result<protocol::CommandAck, RuntimeError>;
    async fn query(&self, query: protocol::RuntimeQuery) -> Result<protocol::QueryResponse, protocol::QueryError>;
    fn subscribe(&self) -> protocol::EventStream;
    async fn snapshot(&self) -> Result<protocol::RuntimeSnapshot, RuntimeError>;
}
```

`dispatch()` 接收 mutation 或异步工作，`query()` 直接返回只读结果，`subscribe()` 传递运行变化，`snapshot()` 提供带事件水位的恢复读模型。助手输出、工具活动、已提交会话事实、资源变化和错误都通过 `agent_runtime_protocol::Event` 传递；query response 不进入事件流。

`agent_runtime_protocol::EventStream` 传输完整的 `agent_runtime_protocol::Event`。外层事件记录负责顺序、路由、关联和重连水位，`msg` 负责业务事实。

```rust
pub struct Event {
    pub event_id: EventId,
    pub sequence: u64,
    pub timestamp: Timestamp,
    pub workspace_id: Option<WorkspaceId>,
    pub session_id: Option<SessionId>,
    pub run_id: Option<RunId>,
    pub command_id: Option<CommandId>,
    pub msg: EventMsg,
}

pub struct CommandAck {
    pub command_id: CommandId,
    pub accepted: bool,
    pub reason: Option<String>,
}
```

`CommandAck` 只表示 `dispatch()` 是否接收了这条协议命令，不是执行结果。对 `ExecuteCommandText` 也是如此：`/status` 的状态内容、`/usage` 的统计、`/model` 的候选或设置完成提示，都应通过后续 command output / business event 返回，而不是塞进 `CommandAck`。

推荐边界：transport/runtime 级无法接收时返回 rejected，例如 runtime 已关闭、workspace 不存在或目标 session id 无效；slash 输入层的用户错误，例如 unknown command、参数非法或当前 phase 不允许，优先返回 accepted，然后发 `command_output_appended { severity: Error }`，让 TUI 和 GUI 在 message 面板中展示一致错误。

`agent_runtime_protocol::EventMsg` 使用分组 enum；UI/wire 层序列化为 flat `snake_case` event type，例如 `run_started`、`tool_call_output_delta`。

wire 形态固定为外层事件 + 内层消息：

```json
{
  "event_id": "evt_...",
  "sequence": 42,
  "session_id": "ses_...",
  "run_id": "run_...",
  "msg": {
    "type": "run_started"
  }
}
```

也就是说，稳定 event type 位于 `msg.type`；外层 `agent_runtime_protocol::Event` 字段是 `workspace_id`、`session_id`、`run_id` 和 `command_id` 的唯一权威位置，负责 routing、ordering、correlation 和 reconnect cursor。`EventMsg` 不重复这些通用坐标；它只保留 message/call/approval/interaction 等局部对象 identity、transition operands 和实际业务数据。

## RuntimeQuery

`RuntimeQuery` 是所有跨 UI/runtime seam 的只读业务查询入口。它按 owner 领域分组，避免形成平铺的巨型 enum：

```rust
pub enum RuntimeQuery {
    Runtime(RuntimeReadQuery),
    Session(SessionQuery),
    Settings(SettingsQuery),
    Resources(ResourceQuery),
    CommandSurface(CommandSurfaceQuery),
    Models(ModelQuery),
    Usage(UsageQuery),
    Diagnostics(DiagnosticsQuery),
}

pub enum SessionQuery {
    List {
        scope: SessionListScope,
        filter: Option<SessionListFilter>,
        cursor: Option<PageCursor>,
        limit: u32,
    },
    GetSummary { session_id: SessionId },
    // future: Messages、Tree、Branches
}

pub enum SettingsQuery {
    GetEffective { session_id: Option<SessionId> },
    GetSchema { section: Option<String> },
    ListProfiles,
}

pub enum ResourceQuery {
    ListSkills { workspace_id: WorkspaceId, cwd: PathBuf },
    GetSkill { workspace_id: WorkspaceId, cwd: PathBuf, skill_name: String },
    GetPromptTemplate { workspace_id: WorkspaceId, cwd: PathBuf, template_name: String },
    GetContextFile { workspace_id: WorkspaceId, cwd: PathBuf, path: PathBuf },
    GetEffectivePrompt { session_id: SessionId },
}

pub enum CommandSurfaceQuery {
    GetCatalog { session_id: Option<SessionId> },
    Suggest { session_id: Option<SessionId>, input: String, cursor: u32 },
}

pub enum ModelQuery {
    ListProviders,
    ListModels { provider_id: Option<String>, include_hidden: bool },
}

pub enum UsageQuery {
    GetSessionStats { session_id: SessionId },
    GetContextUsage { session_id: SessionId },
}

pub enum RuntimeReadQuery {
    GetCapabilities,
}

pub enum DiagnosticsQuery {
    ListRuntime,
    ListResources { workspace_id: WorkspaceId, cwd: Option<PathBuf> },
}
```

每个领域 result type 可以后续持续补充，但外层 envelope 固定：

```rust
pub struct QueryResponse {
    pub as_of_sequence: u64,
    pub revision: Option<QueryRevision>,
    pub data: QueryResult,
}

pub enum QueryResult {
    Runtime(RuntimeReadResult),
    Session(SessionQueryResult),
    Settings(SettingsQueryResult),
    Resources(ResourceQueryResult),
    CommandSurface(CommandSurfaceQueryResult),
    Models(ModelQueryResult),
    Usage(UsageQueryResult),
    Diagnostics(DiagnosticsQueryResult),
}

pub struct QueryError {
    pub kind: QueryErrorKind,
    pub message: String,
}

pub enum QueryErrorKind {
    NotFound,
    InvalidArgument,
    WorkspaceMismatch,
    Unauthorized,
    StaleRevision,
    Unavailable,
}
```

query 的不变量：

- 只读，不改变 session/resource/settings/catalog revision。
- 不创建 turn、不启动 run、不消费 queue、不发布 `agent_runtime_protocol::Event`。
- 读取持久化 session catalog/detail 时不加载完整 `SessionRuntime`。
- 大列表必须分页，cursor opaque 且绑定有效 filters；正文/detail 有大小、trust、path 和 privilege 限制。
- `as_of_sequence` 必须与 owner 的 read projection 在同一同步边界捕获：任何 sequence `<= as_of_sequence` 的相关已发布 mutation 都必须反映在 result 中；更晚变化必须使用更大的 sequence 或领域 revision。不能先读旧数据、再随手读取新 event counter，造成 UI 丢失 invalidation。
- 领域 revision 用于分页 cursor、cache 和 stale 判断；同一 query/result group 必须匹配，不能返回另一领域的 result variant。
- query response 不产生 `CommandAck` 或 `CommandId`。JSON-RPC/Tauri request id 属于 transport，不进入领域协议。

配置遵循 read/query、draft/UI、write/command、change/event 的分工：`SettingsQuery` 返回 effective values、schema、source/provenance 和 editable constraints；UI 持有未提交草稿；配置写入使用 `AgentCommand` 和 expected revision；凭据查询只能返回 redacted status，不能返回 API key、OAuth token 或 auth header。

Rust core 保持统一 `query(RuntimeQuery)` seam；生成给 JSON-RPC/TypeScript SDK 的 adapter 可以暴露 `session/list`、`settings/read`、`usage/session/read` 等方法，并映射回对应领域 query。

## AgentCommand

MVP 命令：

```rust
pub enum AgentCommand {
    OpenWorkspace { path: PathBuf },
    NewSession { workspace_id: WorkspaceId, cwd: Option<PathBuf> },
    OpenSession { session_id: SessionId },
    SubmitPrompt { session_id: SessionId, input: UserInput, delivery: PromptDelivery },
    InvokeSkill { session_id: SessionId, skill_name: String, additional_instructions: Option<String>, delivery: PromptDelivery },
    InvokePromptTemplate { session_id: SessionId, template_name: String, args: Vec<String>, delivery: PromptDelivery },
    ClearQueue { session_id: SessionId },
    AbortRun { run_id: RunId },
    ResumeRun { session_id: SessionId, resume_id: ResumeId },
    DecideToolApproval { approval_id: ApprovalRequestId, session_id: SessionId, run_id: RunId, call_id: ToolCallId, decision: ToolApprovalDecision },
    SetModel { session_id: SessionId, provider_id: String, model_id: String },
    SetThinkingLevel { session_id: SessionId, level: ThinkingLevel },
    SetActiveTools { session_id: SessionId, tool_names: Vec<String> },
    SetQueueMode { session_id: SessionId, queue: QueueKind, mode: QueueMode },
    SetStreamOptions { session_id: SessionId, options: StreamOptions },
    ReloadResources { workspace_id: WorkspaceId, cwd: PathBuf },
    Compact { session_id: SessionId, instructions: Option<String> },
    ExecuteCommandText { session_id: Option<SessionId>, raw: String, prompt_delivery: PromptDelivery },
    ExecuteCatalogCommand { session_id: SessionId, selection: CommandSelection, args: CommandArgs, prompt_delivery: PromptDelivery },
}
```

模型可见输入只使用一个交付入口：

```rust
pub enum PromptDelivery {
    Steer,
    FollowUp,
    NextTurn,
}

pub enum QueueKind {
    Steering,
    FollowUp,
}

pub enum QueueMode {
    All,
    OneAtATime,
}
```

- `Steer`：session idle 时立即启动 run；有 active run 时进入 steering queue，在当前 assistant response 及其完整工具批次结束后、下一次 LLM 调用前注入。若当前 Rig segment 原本将结束，则在 `before_run_finish` 消费并在同一公开 `RunId` 下继续；compaction 或 suspended run 暂时没有安全点时保持排队，直到恢复后的最早模型调用或 post-work continuation。
- `FollowUp`：不修改 active run；当前 work chain、必要 retry/recovery 和 pending session action 完成后，作为后续用户输入启动新 run。session idle 时可立即启动。
- `NextTurn`：不自动启动 run；与下一次显式提交的用户 prompt 一起进入上下文。

`SubmitPrompt`、`InvokeSkill`、`InvokePromptTemplate` 和 prompt-producing slash command 最终都必须归一到这套 `PromptDelivery`。runtime 内部先把它们转换成结构化 `PromptIntent`；queue 保存 resource key/args/附件引用，不保存 raw slash text 或 skill/template body。active `Steer` 由 active `PromptTurn` 展开，`FollowUp` / `NextTurn` 在目标 future turn capture 后展开。`PromptIntent`、`PromptTurn`、`PromptCallProfile` 和 `ModelInputProjection` 都是 runtime 内部类型，不进入公开 wire protocol。

公开协议不再保留独立 `Steer` / `FollowUp` / `NextTurn` 命令，避免两条入口产生不同的 phase guard、queue event 或 hook 行为。`SetQueueMode` 只配置 steering/follow-up 每次安全点消费全部消息还是一条；它不改变 delivery 类型，也不适用于 `NextTurn` 或 `PendingSessionAction`。

推荐 adapter 映射：普通 Enter 使用 `Steer`，显式 follow-up 快捷键或发送模式使用 `FollowUp`，扩展/自动化需要“随下一次用户输入附带”时使用 `NextTurn`。slash command 仍先 resolve；delivery 不能把 `/status` 变成队列消息，也不能把 `/compact` 变成 steer。

`AgentCommand` 是公开协议命令，只表达下游 UI/CLI 可以提交的用户意图。高权限内部 mutation 不属于公开协议，例如直接追加会话消息、替换工具定义、改写会话历史或注入调试状态。它们应放入内部 API：

```rust
pub(crate) enum InternalAgentCommand {
    AppendMessage { session_id: SessionId, message: MessageRecord, trigger_turn: bool },
    SetToolDefinitions { session_id: SessionId, tools: Vec<ToolConfig> },
    MutateSessionHistory { session_id: SessionId, operation: HistoryMutation },
}
```

`InternalAgentCommand` 不能出现在 `RuntimeSnapshot`、`EventMsg`、command catalog、command output action 或 UI-visible interaction 中。

`Compact { session_id, instructions }` 的 command run policy 是 `QueueAfterRun`：

- session 已 `idle` 时立即进入 manual compaction。
- session 有 active run、等待 tool approval、处于 suspended run 或正在自动 retry chain 时，不中止当前 work；`SessionRuntime` 保存一个结构化 `PendingSessionAction::Compact`，并通过 `queue_updated` / `QueueSnapshot.pending_actions` 暴露。
- pending compact 在当前 work 的 terminal handling 完成后（正常完成包含 required stable batch commit；abort/failure 可以没有新 batch）且 terminal event 已发布后、queued steering continuation / follow-up 之前执行；如果还有立即 retry / overflow recovery，则等该 work chain 稳定结束后再执行。`NextTurn` 不会自动启动 continuation。
- 已有 pending compact 时重复提交不追加第二个压缩动作，返回 `CompactAlreadyQueued` 类 command output，保留原 instructions。
- session 已在 compaction phase 时返回 `CompactionAlreadyRunning`；不会排队第二次 compaction。
- `AbortRun`、`ClearQueue`、session close 或 runtime shutdown 会移除 pending compact，并发出更新后的 `queue_updated`；普通 run failure 在不再 retry 时仍执行用户已经排队的 compact。

`Compact` 不能隐式调用 `AbortRun`，也不能清理 tool approval、resume state 或消息队列。`PendingSessionActionView` 是 UI-safe 当前状态投影，不是新的可执行命令载体；真正执行仍使用 `SessionRuntime` 内部保存的结构化 action。

`SessionQuery::List` 一一映射到内部 `SessionManager.list(SessionListRequest { scope, filter, cursor, limit })`，返回轻量会话目录，不加载 session runtime，也不重建完整消息上下文。TUI `/resume` 和 GUI sidebar 复用同一 seam，不能读取本地 index 文件。

```rust
pub enum SessionListScope {
    CurrentWorkspace,
    Workspace { workspace_id: WorkspaceId },
    AllWorkspaces,
    Recent,
}

pub struct SessionListFilter {
    pub query: Option<String>,
}

pub struct PageCursor(pub String);
```

`Recent` 是显式的跨 workspace 全局最近列表；`CurrentWorkspace` 才是 `/resume` 和 sidebar 默认。scope 不塞进 `SessionListFilter`，避免 manager/protocol 双形状。

`RuntimeResourceSnapshot`、`CwdResourceSnapshot` 或 `TurnResourceSnapshot` 都不应作为公开 UI command/query 的输入。公开协议只允许 UI 通过 command 请求 `ReloadResources`、消费 `resources_changed` 摘要，或通过 `ResourceQuery` 读取受控详情。测试、bootstrap 或后续 SDK 如果确实需要注入资源，应走内部 API 或显式标注为 privileged command，不能绕过 `ResourceManager` 的 source info、project trust、overlay policy、diagnostics 和 atomic publish 语义。

`ExecuteCommandText` 只提交原始命令文本；`/...` slash command 是这种文本入口的常见语法。Parse / materialize / suggest / resolve / execute 由 [CommandSurface](command-surface.md) 中的 `CommandManager` 与目标 `SessionRuntime.command` 协调完成。下游 TUI slash input 和 GUI command palette 默认都应走 `ExecuteCommandText`，GUI/TUI 从 catalog 选择某个节点时可以走 `ExecuteCatalogCommand`。两者携带的 `prompt_delivery` 只在命令产生 prompt intent 时生效，例如 `/skill` 或 prompt template；`/status`、`/compact` 等非 prompt command 忽略它并按自己的 `CommandRunPolicy` 执行。普通 `SubmitPrompt` 不应默认解析 slash command，避免用户无法发送以 `/` 开头的普通文本。

`ExecuteCatalogCommand` 只能携带 `CommandSelection`、catalog revision、dynamic bindings、`CommandArgs` 和 prompt-producing command 使用的 `prompt_delivery`，不能携带完整 `AgentCommand` payload 或 handler id。运行时收到后必须基于当前 session context 重新 materialize catalog，并执行 `resolve_for_execution`：command 仍存在、selection 未过期、bindings 仍有效、args 合法、run policy/trust/capability 允许、handler binding 可信，然后才调用 handler。

后续命令：

```rust
AbortCompaction { session_id: SessionId }
AbortRetry { session_id: SessionId }
SetSessionName { session_id: SessionId, name: String }
DeleteSession { session_id: SessionId }
CloseSession { session_id: SessionId }
ImportSession { workspace_id: WorkspaceId, input_path: PathBuf, cwd_override: Option<PathBuf> }
SubmitInteraction { interaction_id: InteractionId, selection: InteractionSelection, form_values: serde_json::Value }
SetToolPolicy { session_id: SessionId, policy: ToolPolicyConfig }
SetToolApprovalMode { session_id: SessionId, mode: ToolApprovalMode }
PatchSettings { target: SettingsTarget, expected_revision: SettingsRevision, patch: SettingsPatch }
ExportSession { session_id: SessionId, format: ExportFormat }
NavigateSessionTree { session_id: SessionId, target_entry_id: EntryId }
ForkSession { session_id: SessionId, from_entry_id: EntryId }
CycleModel { session_id: SessionId, direction: CycleDirection }
CycleThinkingLevel { session_id: SessionId }
SetAutoCompaction { session_id: SessionId, enabled: bool }
SetAutoRetry { session_id: SessionId, enabled: bool }
```

`AbortRun { run_id }` 只取消已经通过 `run_started` 公开的 Agent run。`RunId` 在同一 runtime host 内全局唯一，`AgentRuntime` 可以通过 loaded runtimes 的 current-run lookup 路由到 owner `SessionRuntime`；terminal/stale id 返回 `NoActiveRun` / `StaleRun`，不会取消后来启动的 run。协议不把 `CommandAck` 扩展为 run-id 分配结果，也不引入 `AbortRun { session_id, run_id: Option<RunId> }`。

`AbortRun` 停止当前自动 work chain：它清除尚未消费的 steering/follow-up queue 和 `PendingSessionAction`，保留不会自动启动 run 的 `NextTurn` queue；若清理造成 queue state 变化，则发布清理后的完整 `queue_updated`。已经在 safe point 完成 `UserInput` commit 的 steer 已属于 durable history，不能因 abort 删除或退回 queue。`ClearQueue` 则显式清除 steering、follow-up、next-turn 和 pending actions 全部当前 queue state。

runtime 不实现“abort 后归还队列文本给编辑器”：`CommandAck`、`QueueEvent` 和 snapshot 都不携带 `returned_to_editor`、removed delta 或 editor action。queue 保存的是结构化 prompt intent，不保证保留 raw slash text、原始编辑器文本、光标或 undo state；这些属于 UI-local draft。具体 adapter 可以基于本地 submission history 与 reducer state 提供同一 host 内的 best-effort restore，但该体验不是 core protocol 或 reconnect guarantee。

公开 `AgentCommand` 不包含 `WaitForIdle`，core 也不提供稳定的 `wait_until_settled()` API。等待未来状态既不是 mutation command，也不是立即返回的 query；UI 通过 `RuntimeSnapshot` 和 `session_settled` / `run_finished` 更新 reducer，一次性 CLI、RPC client 和测试如需 imperative await，只能在各自 adapter/test-support 层基于 EventStream 提供 `collect_until(...)` / `wait_for_event(...)` 薄 helper，并采用 subscribe-before-dispatch 或已有 reducer state；这不是任意 background session 的通用 late join，完整订阅/gap 语义属于 BR-044。runtime 内部需要“空闲后执行”的动作必须使用 phase guard、`CommandRunPolicy::QueueAfterRun` 或 typed `PendingSessionAction`，不能通过 wait-then-act 制造竞态。未来只有出现独立 runtime server 的明确需求时，才在 transport/SDK 层评估绑定具体 `RunId` 的 `run/join`，不重新引入 session-level idle command。

`SessionPhase::Turn + current_run = None` 只允许出现在有界的 admission/preflight 和 post-run finalization 窗口。UI 只有在收到 `run_started` 或 snapshot 出现 `current_run.run_id` 后才提供 run-level abort；`CommandAck` 和单独的 `session_phase_changed(turn)` 都不表示 run 已经开始。模型/provider/tool/approval 等可能长时间等待的工作必须发生在 `run_started` 之后；session-level auto retry delay 属于独立 `RetryBackoff` phase，并由 `AbortRetry` 取消，不属于本规则。这里的“有界”表示 admission 只有有限步骤、没有无界 lifecycle/external wait，不是硬实时延迟保证；required session commit 仍遵守 writer 的不可中断与故障契约。UI 若要支持用户在 run id 到达前按下 Ctrl-C，可以只在 adapter 本地按 originating `command_id` 暂存 abort intent，收到 matching `run_started` 后立即发送普通 `AbortRun { run_id }`；若先收到 rejection、`session_phase_changed(idle)`、`session_settled` 或 session close，则清除该本地 intent。

`NavigateSessionTree.target_entry_id` 和 `ForkSession.from_entry_id` 必须是 storage/query 暴露的 committed stable batch boundary。多-entry `ToolRound` 的 assistant message 或中间 tool result 不是合法目标；runtime 返回结构化 invalid target，不能投影或复制半个 stable batch。

`PatchSettings` 是后续配置 mutation 框架：UI 先通过 `SettingsQuery::GetEffective/GetSchema` 读取 effective values、source/provenance、editable constraints 和 revision，在本地维护未提交草稿，再用 expected revision 提交 patch。冲突返回 stale revision；成功后的 effective change 通过对应业务事件更新 UI cache。API key、OAuth token 和 auth header 不得进入普通 `SettingsPatch`，必须走专门的 secure credential seam。

`ResourceQuery` 读取 current `CwdResourceSnapshot.resolved`，不能让 UI adapter 直接读文件。`GetContextFile` 的 path 必须命中当前 cwd snapshot 已登记的 canonical path。active run 的 `GetEffectivePrompt` 读取 active `PromptTurn` 的 redacted profile/provenance；idle preview 如后续支持，必须显式标记为 preview，并且不创建 turn、消费 queue 或改变资源 revision。完整正文默认属于 debug/privileged query，不进入普通 snapshot。

`DecideToolApproval` 只回答已经由 `tool_call_approval_requested` 暴露出来的 pending approval。它不是通用工具执行命令，也不能携带新参数；`SessionRuntime` actor 先确认 `approval_id` 与 `session_id`、`run_id`、`call_id` 匹配 `CurrentRun` projection，再让 `ToolApprovalBroker` resolve 同一个内部 waiter。批准后执行的必须是审批请求中冻结的 prepared args；拒绝后产生 error tool result。

```rust
pub enum ToolApprovalMode {
    AskEveryTime,
    UseRememberedGrants,
    AutoAllow { max_risk: ToolRisk },
    AutoDeny { reason: String },
}

pub enum ToolApprovalDecision {
    ApproveOnce,
    ApproveGrant { scope: ApprovalGrantScope, ttl: Option<Duration> },
    Reject { reason: Option<String> },
}

pub enum ApprovalGrantScope {
    SameCallFingerprint,
    SameToolInRun,
    SameToolInSession,
    SameToolInWorkspace,
}

pub struct ToolCallIndex(pub u32);

pub struct PendingToolApprovalView {
    pub approval_id: ApprovalRequestId,
    pub session_id: SessionId,
    pub run_id: RunId,
    pub call_index: ToolCallIndex,
    pub call_id: ToolCallId,
    pub tool_name: String,
    pub risk: ToolRisk,
    pub reason: String,
    pub preview: ToolApprovalPreview,
    pub created_at: Timestamp,
}
```

`ApprovalGrantScope::SameToolInWorkspace` 是后续保留值，MVP 收到该 scope 必须拒绝 `UnsupportedGrantScope`；不能把它解释成跨 cwd trust/sandbox 的授权。

`PendingToolApprovalView` 是 `SessionRuntime` actor 从 control-class approval-pending update 归约出的 UI-safe projection，放在对应 loaded `SessionSnapshot.current_run.pending_tool_approvals`。`ToolApprovalBroker` 只保存冻结 execution record 与 waiter，不是另一个 UI projection owner。该 view 只暴露审批客户端、命令面板或其他 adapter 回答审批所需的信息，不包含冻结的 `prepared_args`、executor handle、sandbox internals 或 hook-private context。adapter 对该 view 的唯一状态修改入口仍是 `DecideToolApproval { approval_id, session_id, run_id, call_id, decision }`。重复或过期 decision 必须归约为 `ApprovalDecisionOutcome::{AlreadyResolved, StaleRun, StaleCall, NotFound}`，不能导致重复执行。

`OpenSession` 只保证目标 `SessionRuntime` 已加载，不建立 runtime-global 当前会话。`SubmitPrompt`、`InvokeSkill` 和 `InvokePromptTemplate` 只确认请求已接受。是否产生输出要看后续事件。

工作区命令语义按 [ADR 0022](../adr/0022-workspace-is-single-instance-thin-boundary.md)（workspace 是单实例薄边界容器）：

- `OpenWorkspace { path }` 在 `NoWorkspace` 时 accepted 并进入 `Opening`；Ack 不是完成结果。成功后发布 `workspace_opened { workspace }` 并进入 `Open`，失败后发布 `workspace_open_failed { requested_path, error }` 并回到 `NoWorkspace`。`Opening` 中同 canonical root 的重复 command accepted 并共享初始化，异 root 拒绝 `WorkspaceOpening`；`Open` 中同 root 幂等 accepted，异 root 拒绝 `WorkspaceAlreadyOpen`。runtime 不提供 `CloseWorkspace`；"换项目"由 host 重建实例完成。
- `WorkspaceId` 使用 ADR 0022 D2 的规范 `WorkspaceIdV1`（完整 SHA-256、lowercase base32、`ws1_` 前缀）；adapter 不得自行计算。session metadata 必须持久化算法版本和 canonical root；id/root 冲突返回 `WorkspaceIdCollision`。
- `NewSession { workspace_id, cwd }`：`workspace_id` 必须等于当前 open workspace，否则拒绝 `WorkspaceMismatch`；`cwd = None` 使用 root，显式 path canonicalize 后必须位于 root 之下，否则拒绝 `CwdOutsideWorkspace`。
- `OpenSession` 在任何 resource ensure/runtime spawn 前复验 persisted workspace id/root/cwd；跨 workspace 返回 `SessionOutsideWorkspace`，id/root metadata 不一致返回 `SessionWorkspaceCorrupt`，cwd 越界返回 `CwdOutsideWorkspace`。
- `ReloadResources` 与 `ResourceQuery::*` 携带的 `workspace_id` 必须匹配当前 workspace，否则返回 `WorkspaceMismatch`；`cwd` 必须是 root 之下已有 snapshot 或 loaded session 的 cwd。
- `SessionListScope::{AllWorkspaces, Workspace}` 是纯 catalog 查询（不加载 runtime），供浏览与未来导入；`CurrentWorkspace` 依赖 `WorkspaceId` 的路径派生稳定性，在重启后仍能命中同一 root 创建的历史会话。

根据 [ADR 0020](../adr/0020-agent-runtime-has-no-current-session.md)，core 不提供 `FocusSession`、selected/current session 或 sessionless fallback routing。所有 session-scoped command 必须显式携带 `SessionId`。`ExecuteCommandText.session_id = None` 只允许解析和执行 runtime/workspace-scoped command；若解析结果需要 session context，则返回 `SessionRequired` 类 command output，不能从最近打开、唯一 loaded 或 adapter 当前显示的 session 推断目标。客户端可以本地维护 selected session，并在 dispatch 前填入 `session_id`；该状态不进入 core command、event 或 snapshot。`NewSession` 产生的 session id 通过带 originating `command_id` 的 `session_created` / `session_opened` 外层 `Event.session_id` 返回，adapter 可以据此更新自己的 selection。

上述 PascalCase 名称是 `CommandAck.reason` 的稳定 machine-readable reason code，不是可本地化展示文案；adapter 不得匹配任意自然语言。query mismatch 使用 typed `QueryErrorKind::WorkspaceMismatch`。后续若 `CommandAck` 升级为 typed rejection payload，这些 code 原样迁移。

`SetModel.provider_id` 和 `SetModel.model_id` 是 MiniCore 稳定 ID，对应 `ModelSelection { provider_id, model_id }`。它们不是 provider API model name，不是 Rig provider type，也不能携带 base URL、API key、OAuth token 或 auth header。模型执行、custom provider 解析和 auth 注入只发生在 [ModelGateway](model-gateway.md)。

## EventMsg

事件消息 Rust 类型定义：

```rust
pub enum EventMsg {
    Workspace(WorkspaceEvent),
    Session(SessionEvent),
    Run(RunEvent),
    Message(MessageEvent),
    ToolCall(ToolCallEvent),
    Queue(QueueEvent),
    Usage(UsageEvent),
    Resources(ResourcesEvent),
    CommandCatalog(CommandCatalogEvent),
    CommandResult(CommandResultEvent),
    Skill(SkillEvent),
    PromptTemplate(PromptTemplateEvent),
    Compaction(CompactionEvent),
    Retry(RetryEvent),
    Diagnostics(DiagnosticsEvent),
}

pub enum WorkspaceEvent {
    Opened { workspace: WorkspaceSummary },
    OpenFailed { requested_path: PathBuf, error: String },
}

pub enum SessionEvent {
    Created,
    Opened,
    Closed,
    Imported,
    Deleted,
    NameChanged { name: Option<String> },
    TreeChanged { old_leaf_id: Option<EntryId>, new_leaf_id: Option<EntryId> },
    PhaseChanged { phase: SessionPhase },
    ModelChanged { provider_id: String, model_id: String },
    ThinkingLevelChanged { level: ThinkingLevel },
    ToolsChanged { tool_names: Vec<String>, active_tool_names: Vec<String> },
    ActiveToolsChanged { tool_names: Vec<String> },
    StreamOptionsChanged { options: StreamOptions },
    Settled { next_turn_count: u64 },
}

pub enum RunEvent {
    Started,
    Suspended { resume_id: ResumeId, reason: SuspendReason },
    Resumed { resume_id: ResumeId },
    Finished { status: RunTerminalStatus, usage: Option<UsageSummary> },
}

pub enum MessageEvent {
    UserAppended { message_id: MessageId, content: MessageContent },
    AssistantStarted { message_id: MessageId },
    AssistantTextDelta { message_id: MessageId, delta: String },
    AssistantFinished { message_id: MessageId },
    ToolResultAppended { message_id: MessageId, call_id: ToolCallId, is_error: bool },
}

pub enum ToolCallEvent {
    Proposed { call_index: ToolCallIndex, call_id: ToolCallId, name: String, args: serde_json::Value, risk: ToolRisk, requires_approval: bool },
    ApprovalRequested { approval_id: ApprovalRequestId, call_index: ToolCallIndex, call_id: ToolCallId, name: String, risk: ToolRisk, reason: String, preview: ToolApprovalPreview },
    Started { call_index: ToolCallIndex, call_id: ToolCallId },
    OutputDelta { call_index: ToolCallIndex, call_id: ToolCallId, delta: String },
    Finished { call_index: ToolCallIndex, call_id: ToolCallId, result: ToolResultView, is_error: bool },
}

pub enum ToolApprovalPreview {
    Read { path: PathBuf },
    FileWrite { path: PathBuf, creates: bool, overwrites: bool, bytes: u64, diff_preview: Option<String> },
    FileEdit { path: PathBuf, replacements: u32, diff: String },
    Patch { files: Vec<PatchFilePreview>, additions: u64, deletions: u64, diff_preview: Option<String> },
    Bash { command: String, cwd: PathBuf, timeout: Option<u64>, risk_notes: Vec<String> },
}

pub struct PatchFilePreview {
    pub action: PatchFileAction,
    pub path: PathBuf,
    pub move_path: Option<PathBuf>,
    pub additions: u64,
    pub deletions: u64,
}

pub enum PatchFileAction {
    Add,
    Delete,
    Update,
    Move,
}

pub enum QueueEvent {
    Updated {
        follow_up: Vec<QueuedMessageView>,
        steering: Vec<QueuedMessageView>,
        next_turn: Vec<QueuedMessageView>,
        pending_actions: Vec<PendingSessionActionView>,
    },
}

pub struct QueueSnapshot {
    pub follow_up: Vec<QueuedMessageView>,
    pub steering: Vec<QueuedMessageView>,
    pub next_turn: Vec<QueuedMessageView>,
    pub pending_actions: Vec<PendingSessionActionView>,
}

pub enum PendingSessionActionView {
    Compact {
        origin_command_id: CommandId,
        instructions: Option<String>,
    },
}

pub enum UsageEvent {
    Updated {
        run_usage: Option<UsageSummary>,
        session_stats: Option<SessionStatsView>,
        context_usage: Option<ContextUsageView>,
    },
}

pub enum ResourcesEvent {
    ReloadStarted { cwd: PathBuf },
    Changed { cwd: PathBuf, revision: ResourceRevision, skills: Vec<SkillSummary>, prompt_templates: Vec<PromptTemplateSummary>, context_files: Vec<ContextFileSummary>, system_prompt: Option<TextResourceSummary>, append_system_prompts: Vec<TextResourceSummary>, diagnostics: Vec<ResourceDiagnostic> },
}

pub enum CommandCatalogEvent {
    Changed { revision: CommandCatalogRevision, commands: Vec<CommandNodeSummary> },
}

pub enum CommandResultEvent {
    OutputAppended { output_id: CommandOutputId, output: CommandOutput },
    InteractionRequested { interaction_id: InteractionId, request: UiInteractionRequest },
    InteractionResolved { interaction_id: InteractionId, resolution: InteractionResolution },
}

pub enum SkillEvent {
    Invoked { skill_name: String },
}

pub enum PromptTemplateEvent {
    Invoked { template_name: String },
}
```

`QueueEvent::Updated` 始终是替换式完整状态，不附加 consumed/removed delta。abort 后 `steering`、`follow_up` 和 `pending_actions` 为空，`next_turn` 保留原值；`ClearQueue` 实际造成状态变化时，更新后的四者均为空；空队列 no-op 不要求冗余事件。UI 是否把本地保存的原始输入重新放回 editor 是 adapter policy，不是 queue event 的领域语义。

`skill_invoked` / `prompt_template_invoked` 只在结构化 intent 被目标 `PromptTurn.resolve_intent()` 实际展开后发布。idle/future-turn admission 在 `UserInput` commit 成功后、`message_user_appended` 前发布，此时公开 run 尚未开始，外层 `Event.run_id = None`；active Steer 在 current run safe point 展开时使用外层 `Event.run_id = Some(current_run_id)`。FollowUp/NextTurn 入队时不发 invoked；受理状态由 `CommandAck` 和完整 `queue_updated` 表达。展开或 commit 失败不制造 invoked 事件。

```rust
pub enum CompactionEvent {
    Started { reason: CompactionReason },
    Finished { result: Option<CompactionResultView>, aborted: bool, will_retry: bool },
}

pub enum RetryEvent {
    AutoStarted { attempt: u32, max_attempts: u32, delay_ms: u64, error_message: String },
    AutoFinished { status: RetryTerminalStatus, attempt: u32, error: Option<String> },
}

pub enum RetryTerminalStatus {
    Succeeded,
    Failed,
    Aborted,
}

pub enum DiagnosticsEvent {
    RuntimeChanged { diagnostics: Vec<RuntimeDiagnostic> },
    Warning { subject: Option<DiagnosticSubject>, message: String },
    Error { subject: Option<DiagnosticSubject>, message: String, recoverable: bool },
}

pub enum SessionPhase {
    Idle,
    Turn,
    Compaction,
    RetryBackoff,
}

pub enum RunTerminalStatus {
    Completed,
    Failed,
    Aborted,
}

pub enum CurrentRunState {
    Running,
    WaitingApproval,
    Suspended { resume_id: ResumeId, reason: SuspendReason },
}

pub struct ResumeId(pub String);

pub enum SuspendReason {
    WaitingToolApproval,
    WaitingUserInteraction,
    ExternalJobPending,
    UserSuspendedAtSafePoint,
    AfterToolResultCheckpoint,
}

pub enum DiagnosticSubject {
    ToolCall { call_id: ToolCallId },
    ResourcePath { path: PathBuf },
    Model { provider_id: String, model_id: String },
}
```

diagnostics 的 runtime/workspace/session/run/command 归属同样只读取外层 `Event`；`DiagnosticSubject` 只描述 envelope 没有专用坐标的更细业务对象，不能重新引入第二套路由 scope。

命令结果类型用于把 `/status`、`/usage`、`/model`、`/help` 这类用户命令的结果表达成 UI-safe、display-neutral 的语义对象。它们不是模型消息，不进入 `TurnState.messages`，也不替代业务事件；业务事实仍由 `session_model_changed`、`resources_changed`、`usage_updated`、`run_finished` 等事件表达。

`CommandResultEvent` 和 `UiInteractionRequest` 不重复外层 `command_id` / `session_id`。如果 UI 组件需要脱离 event dispatch stack 保存 command output 或 interaction request，adapter 应保存包含 event metadata 与 payload 的本地 view，而不是把裸 `EventMsg` 当成完整事件。若未来某个 payload 需要关联“另一个命令”，必须使用角色明确的 `origin_command_id` 等字段，不能复用通用 `command_id` 制造第二个权威来源。

```rust
pub struct CommandOutput {
    pub title: String,
    pub body: Option<String>,
    pub severity: CommandOutputSeverity,
    pub blocks: Vec<CommandOutputBlock>,
    pub actions: Vec<CommandOutputActionRef>,
}

pub enum CommandOutputSeverity {
    Info,
    Success,
    Warning,
    Error,
}

pub enum CommandOutputBlock {
    Markdown(String),
    KeyValue(Vec<KeyValueRow>),
    List(Vec<String>),
    Table { columns: Vec<String>, rows: Vec<Vec<String>> },
    Code { language: Option<String>, text: String },
}

pub struct KeyValueRow {
    pub key: String,
    pub value: String,
}

pub struct CommandOutputActionRef {
    pub label: String,
    pub action_id: Option<CommandActionId>,
    pub risk: CommandActionRisk,
}

pub enum CommandActionRisk {
    DisplayOnly,
    ReparseCommandText,
    RuntimeOwnedAction,
}

pub struct CommandActionId(pub String);

pub struct UiInteractionRequest {
    pub interaction_id: InteractionId,
    pub title: String,
    pub items: Vec<UiInteractionItem>,
    pub initial_selection: Option<String>,
    pub allow_search: bool,
    pub allow_multi_select: bool,
    pub submit: InteractionSubmitPolicy,
}

pub struct UiInteractionItem {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub selected: bool,
    pub disabled: bool,
    pub disabled_reason: Option<String>,
    pub children: Vec<UiInteractionItem>,
    pub command_ref: Option<CommandNodeRef>,
}

pub enum InteractionSubmitPolicy {
    SubmitInteraction { interaction_id: InteractionId },
    ExecuteCatalogCommand { selection: CommandSelection, args: CommandArgs, prompt_delivery: PromptDelivery },
    DisplayOnly,
}

pub struct CommandSelection {
    pub command_key: CommandKey,
    pub catalog_revision: CommandCatalogRevision,
    pub bindings: BTreeMap<String, String>,
}

pub struct CommandArgs(pub serde_json::Value);

pub struct CommandNodeRef {
    pub command_key: CommandKey,
    pub path: Vec<String>,
}

pub struct CommandKey(pub String);

pub enum InteractionSelection {
    Single { item_id: String },
    Multiple { item_ids: Vec<String> },
    Cancelled,
}

pub enum InteractionResolution {
    Submitted,
    Cancelled,
    Expired,
}

pub struct CommandOutputId(pub String);
pub struct InteractionId(pub String);
```

`UiInteractionRequest` 是 display-neutral 语义请求，不是 UI 组件 API。TUI 可以把候选渲染成终端浮层和键盘上下选择；Tauri/Vue 可以渲染成 modal、command palette 或带搜索的列表。它不能携带完整 `AgentCommand`，也不能携带自由命令模板或 runtime-provided raw command text。交互选择必须回到 runtime 重新执行 `ExecuteCatalogCommand` 或 runtime-tracked `SubmitInteraction`，并再次 materialize/resolve/authorize。用户主动在命令输入框键入的文本仍走 `ExecuteCommandText`。`InteractionResolved` 只在 runtime 跟踪 pending interaction 或实现 `SubmitInteraction` 后需要发布。

usage 和 context usage 的完整语义见 [UsageStats](usage-stats.md)。协议层只定义 UI 需要消费的 view：

```rust
pub struct TokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
}

pub enum UsageSource {
    ProviderReported,
    Estimated,
    Mixed,
}

pub struct UsageSummary {
    pub model_calls: u32,
    pub total: TokenUsage,
    pub by_model: Vec<ModelUsageSummary>,
    pub source: UsageSource,
}

pub struct ModelUsageSummary {
    pub provider_id: String,
    pub model_id: String,
    pub model_calls: u32,
    pub total: TokenUsage,
}

pub enum ContextUsageSource {
    ProviderUsagePlusTrailingEstimate,
    LocalEstimate,
    ProviderReported,
}

pub struct ContextUsageView {
    pub current_tokens: u64,
    pub context_window: Option<u64>,
    pub reserve_tokens: u64,
    pub baseline_tokens: u64,
    pub effective_window_tokens: Option<u64>,
    pub remaining_tokens: Option<u64>,
    pub remaining_percent: Option<f32>,
    pub source: ContextUsageSource,
}

pub struct SessionStatsView {
    pub total_usage: TokenUsage,
    pub last_run: Option<UsageSummary>,
    pub model_calls: u64,
    pub runs: u64,
    pub by_model: Vec<ModelUsageSummary>,
    pub compactions: CompactionStatsView,
}

pub struct CompactionStatsView {
    pub count: u64,
    pub last_tokens_before: Option<u64>,
    pub last_estimated_tokens_after: Option<u64>,
}
```

模型与 provider 相关 view 只面向 UI 展示和选择器，不参与执行路径。执行路径使用 `ModelSelection` / `ActiveModel` / `ModelCallRequest`，其定义见 [ModelGateway](model-gateway.md)。

```rust
pub struct ProviderSummary {
    pub provider_id: String,
    pub display_name: String,
    pub protocol: String,
    pub auth_status: AuthStatusView,
    pub models: Vec<ModelSummary>,
}

pub struct ModelSummary {
    pub provider_id: String,
    pub model_id: String,
    pub display_name: String,
    pub context_window: Option<u64>,
    pub capabilities: ModelCapabilityFlags,
    pub is_available: bool,
    pub unavailable_reason: Option<String>,
}

pub struct ModelCapabilityFlags {
    pub tools: bool,
    pub vision: bool,
    pub streaming: bool,
    pub thinking: bool,
    pub json_schema: bool,
}

pub enum AuthStatusView {
    Available,
    Missing,
    Expired,
    Invalid,
    Unknown,
}
```

`AuthStatusView` 只能表达“可用/缺失/失效”等状态，不包含 key name、token、header、OAuth refresh token 或 provider-specific error body。

`UsageSummary` 表示一次 run 的模型调用消耗；`SessionStatsView` 表示会话累计模型调用消耗；`ContextUsageView` 表示当前上下文窗口占用。`usage_updated` 可以只更新其中一部分，例如压缩结束后只更新 `context_usage`。UI 不应把它们合并成一个数字。

UI/wire event type 映射：

| Rust 内部事件 | Wire event type |
| --- | --- |
| `WorkspaceEvent::Opened` | `workspace_opened` |
| `WorkspaceEvent::OpenFailed` | `workspace_open_failed` |
| `SessionEvent::Created` | `session_created` |
| `SessionEvent::Opened` | `session_opened` |
| `SessionEvent::Closed` | `session_closed` |
| `SessionEvent::Imported` | `session_imported` |
| `SessionEvent::Deleted` | `session_deleted` |
| `SessionEvent::NameChanged` | `session_name_changed` |
| `SessionEvent::TreeChanged` | `session_tree_changed` |
| `SessionEvent::PhaseChanged` | `session_phase_changed` |
| `SessionEvent::ModelChanged` | `session_model_changed` |
| `SessionEvent::ThinkingLevelChanged` | `session_thinking_level_changed` |
| `SessionEvent::ToolsChanged` | `session_tools_changed` |
| `SessionEvent::ActiveToolsChanged` | `session_active_tools_changed` |
| `SessionEvent::StreamOptionsChanged` | `session_stream_options_changed` |
| `SessionEvent::Settled` | `session_settled` |
| `RunEvent::Started` | `run_started` |
| `RunEvent::Suspended` | `run_suspended` |
| `RunEvent::Resumed` | `run_resumed` |
| `RunEvent::Finished` | `run_finished` |
| `MessageEvent::UserAppended` | `message_user_appended` |
| `MessageEvent::AssistantStarted` | `message_assistant_started` |
| `MessageEvent::AssistantTextDelta` | `message_assistant_text_delta` |
| `MessageEvent::AssistantFinished` | `message_assistant_finished` |
| `MessageEvent::ToolResultAppended` | `message_tool_result_appended` |
| `ToolCallEvent::Proposed` | `tool_call_proposed` |
| `ToolCallEvent::ApprovalRequested` | `tool_call_approval_requested` |
| `ToolCallEvent::Started` | `tool_call_started` |
| `ToolCallEvent::OutputDelta` | `tool_call_output_delta` |
| `ToolCallEvent::Finished` | `tool_call_finished` |
| `QueueEvent::Updated` | `queue_updated` |
| `UsageEvent::Updated` | `usage_updated` |
| `ResourcesEvent::ReloadStarted` | `resources_reload_started` |
| `ResourcesEvent::Changed` | `resources_changed` |
| `CommandCatalogEvent::Changed` | `command_catalog_changed` |
| `CommandResultEvent::OutputAppended` | `command_output_appended` |
| `CommandResultEvent::InteractionRequested` | `command_interaction_requested` |
| `CommandResultEvent::InteractionResolved` | `command_interaction_resolved` |
| `SkillEvent::Invoked` | `skill_invoked` |
| `PromptTemplateEvent::Invoked` | `prompt_template_invoked` |
| `CompactionEvent::Started` | `compaction_started` |
| `CompactionEvent::Finished` | `compaction_finished` |
| `RetryEvent::AutoStarted` | `retry_auto_started` |
| `RetryEvent::AutoFinished` | `retry_auto_finished` |
| `DiagnosticsEvent::RuntimeChanged` | `diagnostics_runtime_changed` |
| `DiagnosticsEvent::Warning` | `diagnostics_warning` |
| `DiagnosticsEvent::Error` | `diagnostics_error` |

事件是实时下游界面更新的唯一来源。CLI、Ratatui 和 Tauri/Vue 宿主都应把同一条事件流 reduce 成各自的 UI 状态。`run_finished` 是唯一 run terminal event；不要同时发 `run_aborted` 这类第二终态。`run_suspended` / `run_resumed` 是 current run 的可恢复中间态事件，不是终态。

公共协议不定义 persistence/save-point event。session mutation 统一由内部 `SessionWriter.commit(SessionWriteBatch)` 提交；`message_user_appended`、正常稳定 assistant message 的 `message_assistant_finished`、`message_tool_result_appended`、需要恢复的 session mutation event、成功 `compaction_finished { result: Some(...) }` 和正常 `run_finished { completed }` 只能在对应 batch commit 成功后发布。streaming delta、approval wait、tool progress 和 abort/failure 时用于关闭 UI lifecycle 的 partial assistant events 可以只存在当前 host，不承诺重启恢复。

`command_interaction_resolved` 是后续可选事件。MVP 中候选选择结果可以直接触发新的 `ExecuteCatalogCommand`，不要求 runtime 跟踪 pending interaction；只有实现 `SubmitInteraction` 或需要审计 interaction 关闭/提交时，才需要发布 resolved 事件。用户主动键入命令文本仍可走 `ExecuteCommandText`，但 interaction request 本身不能携带 raw command text。

压缩相关 view 类型只暴露 UI 需要的信息：

```rust
pub enum CompactionReason {
    Manual,
    Threshold,
    Overflow,
}

pub struct CompactionResultView {
    pub summary_preview: String,
    pub first_kept_entry_id: EntryId,
    pub tokens_before: u64,
    pub estimated_tokens_after: Option<u64>,
    pub from_hook: bool,
}
```

完整压缩摘要属于 session entry 内容，默认不进入快照的大列表预览。UI 展开压缩块时应通过运行时命令读取对应 session entry，而不是直接读 JSONL 文件。

## RuntimeSnapshot

`snapshot()` 用于 UI 初始加载、窗口恢复和同一 host 生命周期内的事件流重连/订阅重建。`RuntimeSnapshot` 是运行时当前状态的权威读模型，不是 UI store，不是会话持久化文件，也不在本地单独落盘。关闭窗口后 snapshot 消失；下次打开时由 `AgentRuntime` 从当前内存状态、settings、resources 和 `SessionManager` 的会话目录重新投影生成。

MVP 的 `RuntimeSnapshot` 是 workspace/runtime-scoped，并在同一个 `last_event_sequence` 水位原子投影全部 loaded sessions。`AgentRuntime` 必须通过 event-bus/projection barrier 生成它：barrier 冻结 loaded `SessionRuntimeHandle` membership，向所有 runtime/global event publisher 放入 barrier marker；每个 actor flush marker 之前已接受的 progress/control effect、捕获 projection 后进入 parked 状态，不再发布事件或处理后续 mutation，直到 barrier release。全部 publisher parked 后 coordinator 才读取当前 `last_event_sequence = N`，并把已捕获 projections 与 N 组装为 snapshot；随后统一 release。不能先逐个无保护读取 session、再随手读取一个更晚的 sequence。打开 workspace 后默认不加载旧 session，因此 `loaded_sessions` 为空；持久化 catalog 中未加载的 session 继续通过 `SessionQuery::List` 读取，不会为了生成 snapshot 全部重建。

MVP 的 adapter host 和 `AgentRuntime` 运行在同一个程序上下文、同一个生命周期内；MiniCore runtime 不是独立 daemon/server，不承诺 adapter 失败或断线后 runtime 继续运行再被重新连接。`last_event_sequence` 的 reconnect 语义用于同一 host 生命周期内的初始化、late subscribe、reducer/subscriber 重建和 sequence gap recovery。由于 event sequence 是 runtime-global，snapshot 必须覆盖所有会在该水位前发布事件的 loaded sessions；BR-044 负责进一步定义 subscription start、ring buffer lag 和 gap recovery。未来若采用 session-scoped subscription/cursor，才可以把 all-loaded snapshot 拆成 scoped snapshots。

```rust
pub struct RuntimeSnapshot {
    pub last_event_sequence: u64,

    pub workspace: Option<WorkspaceSummary>,
    pub loaded_sessions: Vec<SessionSnapshot>,
    pub session_catalog: Option<SessionCatalogSummary>,

    pub runtime_diagnostics: Vec<RuntimeDiagnostic>,
    pub model_fallback_message: Option<String>,

    pub providers: Vec<ProviderSummary>,
    pub models: Vec<ModelSummary>,
    pub runtime_command: CommandSnapshot,
}

pub struct SessionSnapshot {
    pub session: SessionSummary,
    pub session_tree: Option<SessionTreeView>,
    pub messages: Vec<MessageView>,
    pub phase: SessionPhase,
    pub current_run: Option<RunView>,
    pub model: Option<ModelSummary>,
    pub thinking_level: ThinkingLevel,
    pub stream_options: StreamOptions,
    pub retry_attempt: u32,
    pub auto_retry_enabled: bool,
    pub auto_compaction_enabled: bool,
    pub active_tools: Vec<ToolSummary>,
    pub tools: Vec<ToolSummary>,
    pub queues: QueueSnapshot,
    pub session_stats: Option<SessionStatsView>,
    pub context_usage: Option<ContextUsageView>,
    pub resources: ResourceSnapshotSummary,
    pub command: CommandSnapshot,
}

pub struct RunView {
    pub run_id: RunId,
    pub state: CurrentRunState,
    pub pending_tool_approvals: Vec<PendingToolApprovalView>,
}

pub struct WorkspaceSummary {
    pub workspace_id: WorkspaceId,
    pub root_path: PathBuf, // canonical root path
    pub display_name: String,
}

pub struct SessionCatalogSummary {
    pub revision: SessionCatalogRevision,
    pub total_count: u64,
}

pub struct SessionCatalogRevision(pub u64);

pub struct ResourceSnapshotSummary {
    pub workspace_id: WorkspaceId,
    pub cwd: PathBuf,
    pub revision: ResourceRevision,
    pub skills: Vec<SkillSummary>,
    pub prompt_templates: Vec<PromptTemplateSummary>,
    pub context_files: Vec<ContextFileSummary>,
    pub system_prompt: Option<TextResourceSummary>,
    pub append_system_prompts: Vec<TextResourceSummary>,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

pub struct CommandSnapshot {
    pub revision: CommandCatalogRevision,
    pub commands: Vec<CommandNodeSummary>,
    pub diagnostics: Vec<CommandDiagnostic>,
}
```

`SessionTreeView` 可以展示 batch 内消息节点，但只有 committed stable batch boundary 才能标记为 navigable/forkable；UI 不能把 `ToolRound` interior entry 直接填入 `NavigateSessionTree` / `ForkSession`。

`loaded_sessions` 只包含当前驻留在 `LoadedSessionRuntimes` 中的 runtime，并按 `SessionSnapshot.session.session_id` 稳定排序；它不是 persistent session list。`OpenSession` / `NewSession` 成功加载 runtime 后加入，`CloseSession` / idle unload 后移除。MVP 从 storage 打开旧会话时以 `SessionPhase::Idle`、`current_run = None` 和空队列启动；`CurrentRunState::Suspended` 只表示同一 host 生命周期内的内存 checkpoint，不用于跨进程恢复未完成 run。model、thinking level、messages、usage stats 等只从 `SessionStorage` 的 committed stable batches 重建；中断时未提交的 partial assistant、pending approval 和 incomplete tool round 不会出现。

每个 `SessionSnapshot` 独立投影自己的 phase、current run、queues、usage、当前 cwd resource summary 和 session-scoped command catalog。多个 session 使用同一 cwd 时允许摘要重复，换取单次 snapshot 的原子可恢复性。当前 host 生命周期内任一 loaded session 如果正在运行且有工具审批等待态，其 `RunView.pending_tool_approvals` 都是恢复审批客户端和后续 `DecideToolApproval` 的权威来源。`RunView.state` 表示 current run 的当前执行状态；`Suspended` 是可恢复暂停状态，不是 terminal status。

`RunView.pending_tool_approvals` 只覆盖所属 session 当前 run 的未决审批；审批 resolved、run abort、run finished 或 session close 后必须从该列表移除。该列表为空表示当前 run 没有需要 adapter 回答的工具审批。多个并行工具调用同时等待审批时，列表可包含多个 `PendingToolApprovalView`，adapter 应逐个用对应 `approval_id` 与 `call_id` 回答。

`CurrentRunState::Suspended` 表示 `Driver` 已在同一 host 生命周期内的可恢复 checkpoint 停住，运行时仅在内存持有 resume state，恢复时应继续同一个未完成 run 的协议 continuation。例如工具结果已经生成但尚未回填给 Rig / provider 时，可以 suspend 并在 resume 后按正式 tool result 协议继续。它不能通过 `run_finished { status: paused }` 表达；`run_finished` 只用于 completed / failed / aborted 这三类终态。

`SessionSnapshot.resources` 是该 loaded session 固定 cwd 的 current effective resource 摘要，用于后续 turn；已经 running 的 run 仍可能使用启动时捕获的旧 `TurnResourceSnapshot`。完整 per-cwd resource catalog 属于 `ResourceSnapshotStore` 内部状态，不额外作为 runtime-global“当前资源”放入 snapshot。需要未加载 cwd 或资源正文时使用受控 `ResourceQuery`。

`SessionSnapshot.command` 是该 loaded session 当前 command context 的 catalog projection。runtime/workspace-scoped command discovery 仍可通过 query 或 runtime-level command output 提供；不存在依赖“当前 session”的单例 `RuntimeSnapshot.command`。

`RuntimeSnapshot.runtime_command` 只包含不需要 session context 的 runtime/workspace-scoped command。`CommandSurfaceQuery::{GetCatalog, Suggest} { session_id: None }` 读取同一 projection；`Some(session_id)` 读取该 loaded session 的完整 catalog。`None` 不能偷偷选择唯一 loaded session，session-scoped candidate/command 必须缺席或返回 `SessionRequired`。

`SessionCatalogSummary` 只说明会话目录 revision 和数量，不是会话清单本身。完整会话列表走 `SessionQuery::List`；TUI `/resume` 与 GUI sidebar 默认使用 `CurrentWorkspace`，并复用 `SessionManager.list(SessionListRequest)`。`SessionIndex` / session catalog 可以作为本地轻量索引或缓存，但不是 `RuntimeSnapshot`，也不由 UI 直接读取。

资源摘要类型只暴露来源和展示信息，不暴露大文本正文：

```rust
pub struct ContextFileSummary { pub path: PathBuf, pub source: ResourceSourceInfo }
pub struct TextResourceSummary { pub source: ResourceSourceInfo, pub kind: TextResourceKind }
pub struct ResourceRevision(pub u64);
```

命令摘要只暴露 autocomplete / command palette / 嵌套菜单需要的信息，不代表执行授权：

```rust
pub struct CommandNodeSummary {
    pub command_key: CommandKey,
    pub parent: Option<CommandKey>,
    pub segment: String,
    pub path: Vec<String>,
    pub title: String,
    pub description: Option<String>,
    pub source: CommandSource,
    pub source_info: Option<ResourceSourceInfo>,
    pub args_hint: Option<String>,
    pub availability: CommandAvailability,
    pub run_policy: CommandRunPolicy,
    pub descriptor_fingerprint: String,
}

pub struct CommandCatalogRevision(pub u64);

pub enum CommandSource {
    Builtin,
    Skill,
    PromptTemplate,
    Model,
    Tool,
    Session,
    Extension,
}

pub enum CommandAvailability {
    Available,
    Disabled { reason: String },
    Hidden,
}

pub enum CommandRunPolicy {
    IdleOnly,
    Immediate,
    QueueAfterRun,
}

pub struct CommandDiagnostic {
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub source: Option<ResourceSourceInfo>,
}
```

技能全文、context file 正文和完整 system prompt 默认不进入 `RuntimeSnapshot`。UI 如果需要预览详情，应使用 `ResourceQuery::GetSkill`、`GetPromptTemplate`、`GetContextFile` 或 privileged `GetEffectivePrompt`。

## Downstream Adapter 调用方式

MiniCore 本仓库只定义 protocol 和 runtime 行为；CLI、TUI、GUI 或 SDK 产品仓库可以按下面方式接入。推荐启动顺序是：创建 `AgentRuntime`，完成 initialize/handshake，先订阅事件流，再 `dispatch(OpenWorkspace { path })`；收到 matching `workspace_opened` 后调用 `snapshot()`，若收到 `workspace_open_failed` 则展示失败并保持未打开状态。adapter 只 reduce `sequence > RuntimeSnapshot.last_event_sequence` 的后续事件。这里的“连接”和“重连”是同一 host 生命周期内的 adapter/subscriber 关系，不是连接独立 daemon。adapter 自己维护 selected session；core 只维护 `loaded_sessions`，完整持久化清单使用 `RuntimeQuery::Session(SessionQuery::List { ... })`。

Ratatui：

```text
keyboard/input event
  → agent_runtime_protocol::AgentCommand
  → AgentRuntime.dispatch
picker/sidebar/detail read
  → agent_runtime_protocol::RuntimeQuery
  → AgentRuntime.query
agent_runtime_protocol::Event stream
  → app state reducer
  → terminal render
```

Tauri：

```text
Vue invoke("submit_prompt"、"invoke_skill"、"open_session")
  → Tauri command
  → AgentRuntime.dispatch
Vue invoke("list_sessions"、"read_settings"、"read_usage")
  → Tauri query adapter
  → AgentRuntime.query
agent_runtime_protocol::Event stream
  → app.emit("runtime-event", event)
  → Vue store reducer
```

JSON-RPC adapter 可以把 `dispatch`、`query`、`snapshot` 映射为 request/response method，把 `subscribe` 映射为持久连接上的 server notifications。transport request id 只关联一次调用；`CommandId`、event sequence 和领域 revision 保持独立。

任何下游适配器都不应直接调用模型提供方、执行工具、读取工作区文件、解析技能文件、读写会话文件，或持有权威会话状态。
