# AgentRuntimeProtocol

`AgentRuntimeProtocol` 是下游 CLI/TUI/GUI adapter 与 `AgentRuntime` 之间的稳定通信协议模块，包含命令、事件、快照和共享视图类型。下游宿主只应依赖这个协议，不依赖 Rig、工具实现或 session 文件。

事件命名、顺序、持久化屏障、重连和各场景生命周期详见 [AgentRuntimeEvents](agent-runtime-events.md)。本文件只保留 UI 需要依赖的协议类型。

Rust 模块名建议使用 `agent_runtime_protocol`。模块内类型保持短名：`Command`、`CommandAck`、`Event`、`EventMsg`、`RuntimeSnapshot`、`EventStream`；跨模块引用时使用完整路径，例如 `agent_runtime_protocol::Command`。如果未来需要在 crate root 或 SDK 中 re-export，可以提供 `AgentRuntimeCommand`、`AgentRuntimeEvent` 等别名，但它们不是新的领域概念。

## 公共 Interface

```rust
use crate::agent_runtime_protocol as protocol;

pub trait AgentRuntime {
    async fn dispatch(&self, command: protocol::Command) -> Result<protocol::CommandAck, RuntimeError>;
    fn subscribe(&self) -> protocol::EventStream;
    async fn snapshot(&self) -> Result<protocol::RuntimeSnapshot, RuntimeError>;
}
```

运行时方法返回确认或快照，不直接返回助手文本。助手输出、工具活动、保存状态、资源变化和错误都通过 `agent_runtime_protocol::Event` 传递。

`agent_runtime_protocol::EventStream` 传输完整的 `agent_runtime_protocol::Event`。它借鉴 Codex 的 `Event { id, msg }` 形态：外层事件记录负责顺序、路由、关联和重连水位，`msg` 负责业务事实。

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

`CommandAck` 只表示 `dispatch()` 是否接收了这条协议命令，不是执行结果。对 `ExecuteSlashCommand` 也是如此：`/status` 的状态内容、`/usage` 的统计、`/model` 的 picker 或设置完成提示，都应通过后续 `CommandPresentationEvent` 或业务事件返回，而不是塞进 `CommandAck`。

推荐边界：transport/runtime 级无法接收时返回 rejected，例如 runtime 已关闭、workspace 不存在或目标 session id 无效；slash 输入层的用户错误，例如 unknown command、参数非法或当前 phase 不允许，优先返回 accepted，然后发 `command_output_appended { severity: Error }`，让 TUI 和 GUI 在 message 面板中展示一致错误。

`agent_runtime_protocol::EventMsg` 使用分组 enum；UI/wire 层序列化为 flat `snake_case` event type，例如 `run_started`、`tool_call_output_delta`。

推荐 wire 形态保持 Codex-like 外层事件 + 内层消息：

```json
{
  "event_id": "evt_...",
  "sequence": 42,
  "session_id": "ses_...",
  "run_id": "run_...",
  "msg": {
    "type": "run_started",
    "run_id": "run_...",
    "session_id": "ses_..."
  }
}
```

也就是说，稳定 event type 位于 `msg.type`；外层 `agent_runtime_protocol::Event` 字段只负责 routing、ordering、correlation 和 reconnect cursor。

## Command

MVP 命令：

```rust
pub enum Command {
    OpenWorkspace { path: PathBuf },
    NewSession { workspace_id: WorkspaceId },
    OpenSession { session_id: SessionId },
    FocusSession { session_id: SessionId },
    ListSessions { scope: SessionListScope, filter: Option<SessionListFilter>, cursor: Option<PageCursor>, limit: usize },
    SubmitPrompt { session_id: SessionId, input: UserInput, delivery: DeliveryMode },
    InvokeSkill { session_id: SessionId, skill_name: String, additional_instructions: Option<String>, delivery: DeliveryMode },
    InvokePromptTemplate { session_id: SessionId, template_name: String, args: Vec<String>, delivery: DeliveryMode },
    Steer { session_id: SessionId, input: UserInput },
    FollowUp { session_id: SessionId, input: UserInput },
    NextTurn { session_id: SessionId, input: UserInput },
    ClearQueue { session_id: SessionId },
    AppendMessage { session_id: SessionId, message: MessageRecord, trigger_turn: bool },
    AbortRun { run_id: RunId },
    WaitForIdle { session_id: SessionId },
    DecideToolApproval { session_id: SessionId, run_id: RunId, call_id: ToolCallId, decision: ToolApprovalDecision },
    SetModel { session_id: SessionId, provider_id: String, model_id: String },
    SetThinkingLevel { session_id: SessionId, level: ThinkingLevel },
    SetTools { session_id: SessionId, tools: Vec<ToolConfig>, active_tool_names: Option<Vec<String>> },
    SetActiveTools { session_id: SessionId, tool_names: Vec<String> },
    SetQueueMode { session_id: SessionId, queue: QueueKind, mode: QueueMode },
    SetStreamOptions { session_id: SessionId, options: StreamOptions },
    ReloadResources { workspace_id: WorkspaceId, cwd: PathBuf },
    Compact { session_id: SessionId, instructions: Option<String> },
    ExecuteSlashCommand { session_id: Option<SessionId>, raw: String, delivery: DeliveryMode },
}
```

`ListSessions` 返回 `SessionManager` 维护的轻量会话目录，不会加载所有 session runtime，也不会重建完整消息上下文。TUI 的 `/resume` 默认使用 `SessionListScope::CurrentWorkspace`；GUI sidebar 也应通过同一查询分页/过滤读取当前 workspace 的 session 清单，而不是直接读取本地 index 文件。`ListSessions` 是读取型 protocol query：SDK / JSON-RPC transport 可以把它实现为 request/response；如果某个 transport 只能走 `dispatch(Command)`，也必须通过明确 response event 或 interaction 返回列表，不能把 session list 塞进 `CommandAck`。

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

`RuntimeResourceSnapshot`、`CwdResourceSnapshot` 或 `TurnResourceSnapshot` 都不应作为公开 UI command 的输入。公开协议只允许 UI 请求 `ReloadResources { workspace_id, cwd }`、消费 `resources_changed` 摘要，或通过 `GetSkill` / `GetPromptTemplate` 这类 detail command 读取受控详情。测试、bootstrap 或后续 SDK 如果确实需要注入资源，应走内部 API 或显式标注为 privileged command，不能绕过 `ResourceManager` 的 source info、project trust、overlay policy、diagnostics 和 atomic publish 语义。

`ExecuteSlashCommand` 只提交原始 `/...` invocation；Parse / Plan / Execute / Present 由运行时的 [CommandSurface](command-surface.md) 模块和 `AgentRuntime` 协调完成。下游 TUI slash input 和 GUI command palette 默认都应走 `ExecuteSlashCommand` 或 runtime-provided interaction submit action；专用设置控件可以直接提交结构化 command，但必须复用同一后端校验、事件和 command presentation 规则。普通 `SubmitPrompt` 不应默认解析 slash command，避免用户无法发送以 `/` 开头的普通文本。

后续命令：

```rust
AbortCompaction { session_id: SessionId }
AbortRetry { session_id: SessionId }
SetSessionName { session_id: SessionId, name: String }
DeleteSession { session_id: SessionId }
CloseSession { session_id: SessionId }
ImportSession { workspace_id: WorkspaceId, input_path: PathBuf, cwd_override: Option<PathBuf> }
ListSkills { workspace_id: WorkspaceId, cwd: PathBuf }
GetSkill { workspace_id: WorkspaceId, cwd: PathBuf, skill_name: String }
GetPromptTemplate { workspace_id: WorkspaceId, cwd: PathBuf, template_name: String }
GetContextFile { workspace_id: WorkspaceId, cwd: PathBuf, path: PathBuf }
GetEffectivePrompt { session_id: SessionId }
CompleteSlashCommandArgs { session_id: Option<SessionId>, command_name: String, prefix: String }
GetSlashCommandCatalog { session_id: Option<SessionId> }
SubmitCommandInteraction { interaction_id: InteractionId, selection: InteractionSelection, form_values: serde_json::Value }
SetToolPolicy { policy: ToolPolicyConfig }
ExportSession { session_id: SessionId, format: ExportFormat }
NavigateSessionTree { session_id: SessionId, target_entry_id: EntryId }
ForkSession { session_id: SessionId, from_entry_id: EntryId }
CycleModel { session_id: SessionId, direction: CycleDirection }
CycleThinkingLevel { session_id: SessionId }
SetAutoCompaction { session_id: SessionId, enabled: bool }
SetAutoRetry { session_id: SessionId, enabled: bool }
GetSessionStats { session_id: SessionId }
GetContextUsage { session_id: SessionId }
```

`GetSkill`、`GetPromptTemplate`、`GetContextFile` 和 `GetEffectivePrompt` 是后续资源详情查询，不是会话运行命令，也不应改变 resource revision。实现时可以把它们做成 request/response 形式的 protocol query，或做成 `dispatch()` 后产生明确 response event 的命令；无论采用哪种 transport，读取都必须经过 `AgentRuntime` / `ResourceManager`，不能让 UI adapter 直接读文件。`GetSkill` / `GetPromptTemplate` 读取 current `CwdResourceSnapshot.resolved`，`GetContextFile` 的 path 必须命中当前 cwd snapshot 已登记的 context file canonical path。`GetEffectivePrompt` 可能包含工具状态和项目上下文，默认应作为 debug/privileged query，而不是普通 snapshot 字段。

`DecideToolApproval` 只回答已经由 `tool_call_approval_requested` 暴露出来的 pending approval。它不是通用工具执行命令，也不能携带新参数；`ToolApprovalBroker` 必须确认 `session_id`、`run_id`、`call_id` 仍匹配当前 pending tool call。批准后执行的必须是审批请求中冻结的 prepared args；拒绝后产生 error tool result。

```rust
pub enum ToolApprovalDecision {
    Approve,
    Reject { reason: Option<String> },
}
```

`OpenSession` 会在需要时加载 `SessionRuntime`，并通常使它成为 focused session；`FocusSession` 只改变 runtime-visible focus，不要求重新加载已打开会话。`SubmitPrompt`、`InvokeSkill` 和 `InvokePromptTemplate` 只确认请求已接受。是否产生输出要看后续事件。

`SetModel.provider_id` 和 `SetModel.model_id` 是 MiniCore 稳定 ID，对应 `ModelSelection { provider_id, model_id }`。它们不是 provider API model name，不是 Rig provider type，也不能携带 base URL、API key、OAuth token 或 auth header。模型执行、custom provider 解析和 auth 注入只发生在 [ModelGateway](model-gateway.md)。

## EventMsg

事件消息 Rust 类型定义：

```rust
pub enum EventMsg {
    Session(SessionEvent),
    Run(RunEvent),
    Message(MessageEvent),
    ToolCall(ToolCallEvent),
    Queue(QueueEvent),
    Usage(UsageEvent),
    Resources(ResourcesEvent),
    SlashCommand(SlashCommandEvent),
    CommandPresentation(CommandPresentationEvent),
    Skill(SkillEvent),
    PromptTemplate(PromptTemplateEvent),
    Compaction(CompactionEvent),
    Retry(RetryEvent),
    Persistence(PersistenceEvent),
    Diagnostics(DiagnosticsEvent),
}

pub enum SessionEvent {
    Created { session_id: SessionId, workspace_id: WorkspaceId },
    Opened { session_id: SessionId },
    Closed { session_id: SessionId },
    Imported { session_id: SessionId, workspace_id: WorkspaceId },
    Deleted { session_id: SessionId },
    FocusChanged { previous_session_id: Option<SessionId>, focused_session_id: Option<SessionId> },
    NameChanged { session_id: SessionId, name: Option<String> },
    TreeChanged { session_id: SessionId, old_leaf_id: Option<EntryId>, new_leaf_id: Option<EntryId> },
    PhaseChanged { session_id: SessionId, phase: SessionPhase },
    ModelChanged { session_id: SessionId, provider_id: String, model_id: String },
    ThinkingLevelChanged { session_id: SessionId, level: ThinkingLevel },
    ToolsChanged { session_id: SessionId, tool_names: Vec<String>, active_tool_names: Vec<String> },
    ActiveToolsChanged { session_id: SessionId, tool_names: Vec<String> },
    StreamOptionsChanged { session_id: SessionId, options: StreamOptions },
    Settled { session_id: SessionId, next_turn_count: usize },
}

pub enum RunEvent {
    Started { run_id: RunId, session_id: SessionId },
    Finished { run_id: RunId, status: RunTerminalStatus, usage: Option<UsageSummary> },
}

pub enum MessageEvent {
    UserAppended { message_id: MessageId, content: MessageContent },
    AssistantStarted { message_id: MessageId },
    AssistantTextDelta { message_id: MessageId, delta: String },
    AssistantFinished { message_id: MessageId },
    ToolResultAppended { message_id: MessageId, call_id: ToolCallId, is_error: bool },
}

pub enum ToolCallEvent {
    Proposed { call_id: ToolCallId, name: String, args: serde_json::Value, risk: ToolRisk, requires_approval: bool },
    ApprovalRequested { call_id: ToolCallId, name: String, risk: ToolRisk, reason: String, preview: ToolApprovalPreview },
    Started { call_id: ToolCallId },
    OutputDelta { call_id: ToolCallId, delta: String },
    Finished { call_id: ToolCallId, result: ToolResultView, is_error: bool },
}

pub enum ToolApprovalPreview {
    Read { path: PathBuf },
    FileWrite { path: PathBuf, creates: bool, overwrites: bool, bytes: u64, diff_preview: Option<String> },
    FileEdit { path: PathBuf, replacements: usize, diff: String },
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
    Updated { follow_up: Vec<QueuedMessageView>, steering: Vec<QueuedMessageView>, next_turn: Vec<QueuedMessageView> },
}

pub enum UsageEvent {
    Updated {
        session_id: SessionId,
        run_id: Option<RunId>,
        run_usage: Option<UsageSummary>,
        session_stats: Option<SessionStatsView>,
        context_usage: Option<ContextUsageView>,
    },
}

pub enum ResourcesEvent {
    ReloadStarted { workspace_id: WorkspaceId, cwd: PathBuf },
    Changed { workspace_id: WorkspaceId, cwd: PathBuf, revision: ResourceRevision, skills: Vec<SkillSummary>, prompt_templates: Vec<PromptTemplateSummary>, context_files: Vec<ContextFileSummary>, system_prompt: Option<TextResourceSummary>, append_system_prompts: Vec<TextResourceSummary>, diagnostics: Vec<ResourceDiagnostic> },
}

pub enum SlashCommandEvent {
    CatalogChanged { workspace_id: WorkspaceId, session_id: Option<SessionId>, revision: SlashCommandCatalogRevision, commands: Vec<SlashCommandSummary> },
}

pub enum CommandPresentationEvent {
    OutputAppended { command_id: CommandId, session_id: Option<SessionId>, output_id: CommandOutputId, output: CommandOutput },
    InteractionRequested { command_id: CommandId, session_id: Option<SessionId>, interaction_id: InteractionId, request: UiInteractionRequest },
    InteractionResolved { interaction_id: InteractionId, resolution: InteractionResolution },
}

pub enum SkillEvent {
    Invoked { session_id: SessionId, run_id: RunId, skill_name: String },
}

pub enum PromptTemplateEvent {
    Invoked { session_id: SessionId, run_id: RunId, template_name: String },
}

pub enum CompactionEvent {
    Started { session_id: SessionId, reason: CompactionReason },
    Finished { session_id: SessionId, result: Option<CompactionResultView>, aborted: bool, will_retry: bool },
}

pub enum RetryEvent {
    AutoStarted { session_id: SessionId, attempt: u32, max_attempts: u32, delay_ms: u64, error_message: String },
    AutoFinished { session_id: SessionId, success: bool, attempt: u32, final_error: Option<String> },
}

pub enum PersistenceEvent {
    SavePoint { session_id: SessionId, had_pending_mutations: bool },
}

pub enum DiagnosticsEvent {
    RuntimeChanged { diagnostics: Vec<RuntimeDiagnostic> },
    Warning { scope: EventScope, message: String },
    Error { scope: EventScope, message: String, recoverable: bool },
}

pub enum RunTerminalStatus {
    Completed,
    Failed,
    Aborted,
    Paused,
}

pub enum EventScope {
    Runtime,
    Workspace { workspace_id: WorkspaceId },
    Session { session_id: SessionId },
    Run { run_id: RunId },
    ToolCall { call_id: ToolCallId },
    ResourceReload { workspace_id: WorkspaceId },
}
```

命令呈现类型用于把 `/status`、`/usage`、`/model`、`/help` 这类用户命令的结果表达成 UI 可渲染的语义对象。它们不是模型消息，不进入 `TurnState.messages`，也不替代业务事件；业务事实仍由 `session_model_changed`、`resources_changed`、`usage_updated`、`run_finished` 等事件表达。

如果 `CommandPresentationEvent` 或 `UiInteractionRequest` 中携带 `command_id`，它必须与外层 `agent_runtime_protocol::Event.command_id` 一致。外层 `Event` 仍是 routing、ordering 和 correlation 的权威位置；内层字段只是为了 UI 组件在脱离事件上下文渲染时仍能关联原始命令。

```rust
pub struct CommandOutput {
    pub title: String,
    pub body: Option<String>,
    pub severity: CommandOutputSeverity,
    pub blocks: Vec<CommandOutputBlock>,
    pub actions: Vec<CommandOutputAction>,
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

pub struct CommandOutputAction {
    pub label: String,
    pub command: Option<CommandAction>,
}

pub enum CommandAction {
    ExecuteSlashCommand { raw: String },
    DispatchCommand(Command),
    OpenInteraction { interaction_id: InteractionId },
}

pub struct UiInteractionRequest {
    pub interaction_id: InteractionId,
    pub command_id: CommandId,
    pub session_id: Option<SessionId>,
    pub title: String,
    pub kind: UiInteractionKind,
    pub items: Vec<UiInteractionItem>,
    pub initial_selection: Option<String>,
    pub allow_search: bool,
    pub allow_multi_select: bool,
    pub submit: UiInteractionSubmit,
}

pub enum UiInteractionKind {
    Picker,
    MultiSelect,
    Menu,
    Form,
    DetailView,
}

pub struct UiInteractionItem {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub selected: bool,
    pub disabled: bool,
    pub disabled_reason: Option<String>,
    pub children: Vec<UiInteractionItem>,
    pub metadata: serde_json::Value,
}

pub struct CommandTemplate {
    pub command_name: String,
    pub args_template: serde_json::Value,
}

pub enum UiInteractionSubmit {
    ExecuteSlashCommandTemplate { template: String },
    DispatchCommandTemplate { command: CommandTemplate },
    SubmitInteraction { interaction_id: InteractionId },
}

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

`UiInteractionRequest` 是语义请求，不是 UI 组件 API。TUI 可以把 `Picker` 渲染成终端浮层和键盘上下选择；Tauri/Vue 可以渲染成 modal、command palette 或带搜索的列表。`Popup` 是 command output 的展示目标提示，不代表必须等待用户提交的 interaction。MVP 中，交互选择可以通过 `ExecuteSlashCommandTemplate` 重新提交 slash command，例如 model picker 选中后提交 `/model {item.id}`；复杂表单再使用后续的 `SubmitCommandInteraction`。`InteractionResolved` 只在 runtime 跟踪 pending interaction 或实现 `SubmitCommandInteraction` 后需要发布。

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
| `SessionEvent::Created` | `session_created` |
| `SessionEvent::Opened` | `session_opened` |
| `SessionEvent::Closed` | `session_closed` |
| `SessionEvent::Imported` | `session_imported` |
| `SessionEvent::Deleted` | `session_deleted` |
| `SessionEvent::FocusChanged` | `session_focus_changed` |
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
| `SlashCommandEvent::CatalogChanged` | `slash_commands_changed` |
| `CommandPresentationEvent::OutputAppended` | `command_output_appended` |
| `CommandPresentationEvent::InteractionRequested` | `command_interaction_requested` |
| `CommandPresentationEvent::InteractionResolved` | `command_interaction_resolved` |
| `SkillEvent::Invoked` | `skill_invoked` |
| `PromptTemplateEvent::Invoked` | `prompt_template_invoked` |
| `CompactionEvent::Started` | `compaction_started` |
| `CompactionEvent::Finished` | `compaction_finished` |
| `RetryEvent::AutoStarted` | `retry_auto_started` |
| `RetryEvent::AutoFinished` | `retry_auto_finished` |
| `PersistenceEvent::SavePoint` | `persistence_save_point` |
| `DiagnosticsEvent::RuntimeChanged` | `diagnostics_runtime_changed` |
| `DiagnosticsEvent::Warning` | `diagnostics_warning` |
| `DiagnosticsEvent::Error` | `diagnostics_error` |

事件是实时下游界面更新的唯一来源。CLI、Ratatui 和 Tauri/Vue 宿主都应把同一条事件流 reduce 成各自的 UI 状态。`run_finished` 是唯一 run terminal event；不要同时发 `run_aborted` 这类第二终态。

`command_interaction_resolved` 是后续可选事件。MVP 中 picker/menu/form 的选择结果可以直接触发新的 `ExecuteSlashCommand`，不要求 runtime 跟踪 pending interaction；只有实现 `SubmitCommandInteraction` 或需要审计 interaction 关闭/提交时，才需要发布 resolved 事件。

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

MVP 的 `RuntimeSnapshot` 是 workspace/runtime-scoped：打开 workspace 后默认不聚焦任何 session，`active_session` 为空。TUI 可在用户执行 `/resume` 时再通过 `ListSessions` 获取当前 workspace 的会话清单；GUI 如果需要 sidebar，也应通过 `ListSessions` 或会话目录 query 获取清单，而不是要求 `RuntimeSnapshot` 默认携带完整 `Vec<SessionSummary>`。

MVP 的 UI host 和 `AgentRuntime` 运行在同一个程序上下文、同一个生命周期内；MiniCore runtime 不是独立 daemon/server，不承诺 UI adapter 失败或断线后 runtime 继续运行再被重新连接。`last_event_sequence` 的 reconnect 语义用于同一 host 生命周期内的初始化、late subscribe、reducer/subscriber 重建和 sequence gap recovery。它不要求 `RuntimeSnapshot` 覆盖所有非 active/background session 的完整运行态；如果未来引入独立 runtime server、多窗口共享 runtime 或 daemon 模式，需要重新设计 all-loaded-session snapshot 或 scoped event cursor。

```rust
pub struct RuntimeSnapshot {
    pub last_event_sequence: u64,

    pub workspace: Option<WorkspaceSummary>,
    pub active_session_id: Option<SessionId>,
    pub active_session: Option<SessionSnapshot>,
    pub session_catalog: Option<SessionCatalogSummary>,

    pub runtime_diagnostics: Vec<RuntimeDiagnostic>,
    pub model_fallback_message: Option<String>,

    pub providers: Vec<ProviderSummary>,
    pub models: Vec<ModelSummary>,

    pub resources: ResourceSnapshotSummary,
    pub command_surface: CommandSurfaceSnapshot,
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
}

pub struct SessionCatalogSummary {
    pub revision: SessionCatalogRevision,
    pub total_count: u64,
}

pub struct SessionCatalogRevision(pub u64);

pub struct ResourceSnapshotSummary {
    pub workspace_id: Option<WorkspaceId>,
    pub cwd: Option<PathBuf>,
    pub revision: Option<ResourceRevision>,
    pub skills: Vec<SkillSummary>,
    pub prompt_templates: Vec<PromptTemplateSummary>,
    pub context_files: Vec<ContextFileSummary>,
    pub system_prompt: Option<TextResourceSummary>,
    pub append_system_prompts: Vec<TextResourceSummary>,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

pub struct CommandSurfaceSnapshot {
    pub slash_commands: Vec<SlashCommandSummary>,
}
```

`active_session_id` 是 runtime-visible 的当前默认会话目标；窗口刚打开且用户尚未选择 session 时为 `None`。`SessionSnapshot` 只描述已打开的 active session，恢复旧会话时默认以 `SessionPhase::Idle`、`current_run = None` 和空队列启动；model、thinking level、messages、usage stats 等来自 `SessionStorage` 的持久化事实重建。UI 的 usage 面板应以 `SessionSnapshot.session_stats`、`SessionSnapshot.context_usage` 和后续 `usage_updated` 为权威，不应自行从消息内容估算 token。

`RuntimeSnapshot.resources` 是当前 active/focused cwd 的资源摘要；没有 active session 时可以为空摘要。完整 per-cwd resource catalog 属于 `ResourceSnapshotStore` 内部状态，不默认放入 runtime snapshot。GUI 如果需要展示多个 cwd 的资源状态，应通过后续明确 query，而不是要求 `RuntimeSnapshot` 携带所有 cwd 的完整资源摘要。

`SessionCatalogSummary` 只说明会话目录 revision 和数量，不是会话清单本身。完整会话列表走 `ListSessions`；TUI 的 `/resume` 默认使用 `SessionListScope::CurrentWorkspace`，GUI sidebar 也复用同一查询。`SessionIndex` / session catalog 可以作为 `SessionManager` 的本地轻量索引或缓存，但它不是 `RuntimeSnapshot`，也不由 UI 直接读取。

资源摘要类型只暴露来源和展示信息，不暴露大文本正文：

```rust
pub struct ContextFileSummary { pub path: PathBuf, pub source: ResourceSourceInfo }
pub struct TextResourceSummary { pub source: ResourceSourceInfo, pub kind: TextResourceKind }
pub struct ResourceRevision(pub u64);
```

斜杠命令摘要只暴露 autocomplete / command palette 需要的信息，不代表执行授权：

```rust
pub struct SlashCommandSummary {
    pub name: String,
    pub aliases: Vec<String>,
    pub description: Option<String>,
    pub source: SlashCommandSource,
    pub source_info: Option<ResourceSourceInfo>,
    pub argument_hint: Option<String>,
    pub presentation_hint: Option<CommandPresentationHint>,
    pub availability: SlashCommandAvailability,
    pub phase_policy: SlashCommandPhasePolicy,
}

pub enum CommandPresentationHint {
    MessagePanel,
    Popup,
    Picker,
    Menu,
    Form,
    DetailView,
}

pub struct SlashCommandCatalogRevision(pub u64);

pub enum SlashCommandSource {
    Builtin,
    Skill,
    PromptTemplate,
    Extension,
    ModelServiceTier,
}

pub enum SlashCommandAvailability {
    Available,
    Disabled { reason: String },
    Hidden,
}

pub enum SlashCommandPhasePolicy {
    IdleOnly,
    AllowedDuringRun,
    QueueAsSteer,
    QueueAsFollowUp,
    ImmediateRuntimeAction,
}
```

技能全文、context file 正文和完整 system prompt 默认不进入 `RuntimeSnapshot`。UI 如果需要预览技能详情，应通过 `GetSkill` 命令请求；后续可增加 `GetPromptTemplate`、`GetContextFile` 或 `GetEffectivePrompt`。

## Downstream Adapter 调用方式

MiniCore 本仓库只定义 protocol 和 runtime 行为；CLI、TUI、GUI 产品仓库可以按下面方式接入。推荐启动顺序是：在宿主进程内创建或取得 `AgentRuntime`，完成 initialize/handshake，订阅事件流，`dispatch(OpenWorkspace { path })`，再调用 `snapshot()` 取得 `RuntimeSnapshot`。UI 只 reduce `sequence > RuntimeSnapshot.last_event_sequence` 的后续事件。这里的“连接”和“重连”是同一 host 生命周期内的 adapter/subscriber 关系，不是连接一个可独立存活的 runtime daemon。TUI 可以保持 `active_session = None` 并等待用户 `/resume`；GUI 如果需要 sidebar，应单独调用 `ListSessions`。

Ratatui：

```text
keyboard/input event
  → agent_runtime_protocol::Command
  → AgentRuntime.dispatch
agent_runtime_protocol::Event stream
  → app state reducer
  → terminal render
```

Tauri：

```text
Vue invoke("submit_prompt"、"invoke_skill"、"open_session" 或 "list_sessions")
  → Tauri command
  → AgentRuntime.dispatch
agent_runtime_protocol::Event stream
  → app.emit("runtime-event", event)
  → Vue store reducer
```

任何下游适配器都不应直接调用模型提供方、执行工具、读取工作区文件、解析技能文件、读写会话文件，或持有权威会话状态。
